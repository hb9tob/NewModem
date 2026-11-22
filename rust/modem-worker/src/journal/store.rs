//! On-disk layout of the journal, and the bundle that gets submitted.
//!
//! ```text
//! <root>/journal-<unix>-<pid>/
//!     lock             held (OS file lock) while the application runs
//!     header.json      one line: versions, host, settings
//!     seg-00000.jsonl  ~5 MB segments, oldest dropped first
//!     closed           present after an orderly shutdown
//!     handled          present once sent, or declined by the operator
//! ```
//!
//! A directory with neither a held lock nor a `closed` marker is a session
//! that crashed or was killed: the interesting case, offered on next launch.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;

const DIR_PREFIX: &str = "journal-";
const LOCK: &str = "lock";
const HEADER: &str = "header.json";
const CLOSED: &str = "closed";
const HANDLED: &str = "handled";

/// Disk limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub segment_bytes: u64,
    /// Budget shared by all the sessions kept on disk.
    pub total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            segment_bytes: 5 * 1024 * 1024,
            total_bytes: 100 * 1024 * 1024,
        }
    }
}

/// Append-only segmented writer of one application session.
pub struct SessionWriter {
    dir: PathBuf,
    limits: Limits,
    /// Keeps the OS lock for the lifetime of the session.
    _lock: File,
    out: BufWriter<File>,
    index: u32,
    written: u64,
}

impl SessionWriter {
    pub fn create(root: &Path, header: &serde_json::Value, limits: Limits) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        prune_sessions(root, limits.total_bytes);
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let dir = root.join(format!("{DIR_PREFIX}{unix:013}-{}", std::process::id()));
        fs::create_dir(&dir)?;
        let lock = File::create(dir.join(LOCK))?;
        lock.try_lock().map_err(io::Error::from)?;
        fs::write(dir.join(HEADER), format!("{header}\n"))?;
        let out = BufWriter::new(File::create(segment_path(&dir, 0))?);
        Ok(Self {
            dir,
            limits,
            _lock: lock,
            out,
            index: 0,
            written: 0,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one record (`line` has no trailing newline).
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.out.write_all(line.as_bytes())?;
        self.out.write_all(b"\n")?;
        self.written += line.len() as u64 + 1;
        if self.written >= self.limits.segment_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    /// Push buffered records to the OS. No fsync: this may be an SD card.
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn close(mut self) -> io::Result<()> {
        self.out.flush()?;
        File::create(self.dir.join(CLOSED)).map(|_| ())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.out.flush()?;
        let next = File::create(segment_path(&self.dir, self.index + 1))?;
        self.index += 1;
        self.out = BufWriter::new(next);
        self.written = 0;
        // Make room for the segment being started.
        let budget = self.limits.total_bytes.saturating_sub(self.limits.segment_bytes);
        let mut segs = segments(&self.dir);
        segs.pop();
        let mut total: u64 = segs.iter().map(|p| file_len(p)).sum();
        for old in &segs {
            if total <= budget {
                break;
            }
            let len = file_len(old);
            if fs::remove_file(old).is_ok() {
                total -= len;
            }
        }
        Ok(())
    }
}

fn segment_path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("seg-{index:05}.jsonl"))
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seg-") && n.ends_with(".jsonl"))
        })
        .collect();
    out.sort();
    out
}

/// Journal directories under `root`, oldest first. Only directories this
/// module created qualify: the prefix plus a header or lock file.
pub fn sessions(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        // `file_type` does not follow symlinks.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name().to_string_lossy().starts_with(DIR_PREFIX))
        .map(|e| e.path())
        .filter(|p| p.join(LOCK).is_file())
        .collect();
    out.sort();
    out
}

/// True while the application that owns the session is still running.
fn is_live(dir: &Path) -> bool {
    match OpenOptions::new().write(true).open(dir.join(LOCK)) {
        // The lock is released again when `f` drops.
        Ok(f) => f.try_lock().is_err(),
        Err(_) => false,
    }
}

/// Sessions that ended without an orderly shutdown and were never offered.
pub fn unfinished(root: &Path) -> Vec<PathBuf> {
    sessions(root)
        .into_iter()
        .filter(|d| !d.join(CLOSED).exists() && !d.join(HANDLED).exists() && !is_live(d))
        .collect()
}

/// Do not offer this session again.
pub fn mark_handled(dir: &Path) -> io::Result<()> {
    File::create(dir.join(HANDLED)).map(|_| ())
}

/// Uncompressed size of a session.
pub fn session_bytes(dir: &Path) -> u64 {
    file_len(&dir.join(HEADER)) + segments(dir).iter().map(|p| file_len(p)).sum::<u64>()
}

/// Delete the files this module wrote, then the directory if that emptied
/// it. Never a recursive delete: the root lives in the operator's folders.
fn remove_session(dir: &Path) {
    for seg in segments(dir) {
        let _ = fs::remove_file(seg);
    }
    for name in [HEADER, CLOSED, HANDLED, LOCK] {
        let _ = fs::remove_file(dir.join(name));
    }
    let _ = fs::remove_dir(dir);
}

/// Drop the oldest finished sessions until `root` fits in `budget`.
fn prune_sessions(root: &Path, budget: u64) {
    let dirs = sessions(root);
    let mut total: u64 = dirs.iter().map(|d| session_bytes(d)).sum();
    for dir in &dirs {
        if total <= budget {
            break;
        }
        if is_live(dir) {
            continue;
        }
        total -= session_bytes(dir);
        remove_session(dir);
    }
}

/// A journal ready for submission: a multi-member gzip of JSON lines.
#[derive(Debug)]
pub struct Bundle {
    pub gz: Vec<u8>,
    /// Older segments left out to respect the size limit.
    pub omitted_segments: usize,
}

/// Compress header + newest segments of a session within `max_gz` bytes.
///
/// Each file is its own gzip member, so "keep the newest that fit" needs no
/// recompression; `gunzip` and Python's `gzip` read such files as one.
pub fn bundle(dir: &Path, max_gz: usize) -> io::Result<Bundle> {
    let header = gzip(&fs::read(dir.join(HEADER))?)?;
    let segs = segments(dir);
    let mut members: Vec<Vec<u8>> = Vec::new();
    let mut size = header.len();
    // Room for the truncation record.
    let limit = max_gz.saturating_sub(128);
    for seg in segs.iter().rev() {
        let mut raw = fs::read(seg)?;
        // A crash can leave half a record at the end.
        let complete = raw.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        raw.truncate(complete);
        let member = gzip(&raw)?;
        if size + member.len() > limit {
            break;
        }
        size += member.len();
        members.push(member);
    }
    let omitted_segments = segs.len() - members.len();

    let mut gz = header;
    if omitted_segments > 0 {
        let note = serde_json::json!({"kind": "truncated", "omitted_segments": omitted_segments});
        gz.extend(gzip(format!("{note}\n").as_bytes())?);
    }
    for member in members.iter().rev() {
        gz.extend_from_slice(member);
    }
    Ok(Bundle {
        gz,
        omitted_segments,
    })
}

fn gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::MultiGzDecoder;
    use serde_json::json;
    use std::io::Read;

    fn unpack(gz: &[u8]) -> String {
        let mut s = String::new();
        MultiGzDecoder::new(gz).read_to_string(&mut s).unwrap();
        s
    }

    fn small() -> Limits {
        Limits {
            segment_bytes: 1000,
            total_bytes: 4000,
        }
    }

    #[test]
    fn rotation_keeps_the_newest_within_budget() {
        let root = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::create(root.path(), &json!({"kind": "header"}), small()).unwrap();
        for i in 0..1000 {
            w.write_line(&format!("{{\"i\":{i}}}")).unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.close().unwrap();
        assert!(session_bytes(&dir) <= 4100);
        let text = unpack(&bundle(&dir, 1 << 20).unwrap().gz);
        assert!(text.starts_with("{\"kind\":\"header\"}"));
        assert!(text.contains("{\"i\":999}"));
        assert!(!text.contains("{\"i\":0}"));
    }

    #[test]
    fn oversized_bundle_keeps_the_tail_and_says_so() {
        let root = tempfile::tempdir().unwrap();
        let limits = Limits {
            segment_bytes: 2000,
            total_bytes: 1 << 20,
        };
        let mut w = SessionWriter::create(root.path(), &json!({"kind": "header"}), limits).unwrap();
        for i in 0..3000u64 {
            // Poorly compressible payload.
            w.write_line(&format!("{{\"i\":{i},\"x\":{}}}", i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
                .unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.close().unwrap();
        let b = bundle(&dir, 8000).unwrap();
        assert!(b.gz.len() <= 8000);
        assert!(b.omitted_segments > 0);
        let text = unpack(&b.gz);
        assert!(text.contains("\"truncated\""));
        assert!(text.contains("{\"i\":2999,"));
    }

    #[test]
    fn only_dead_unclosed_sessions_are_offered() {
        let root = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::create(root.path(), &json!({}), Limits::default()).unwrap();
        w.write_line("{}").unwrap();
        w.flush().unwrap();
        assert!(unfinished(root.path()).is_empty(), "live session offered");

        let dir = w.dir().to_path_buf();
        drop(w); // Killed: lock released, no `closed` marker.
        assert_eq!(unfinished(root.path()), vec![dir.clone()]);

        mark_handled(&dir).unwrap();
        assert!(unfinished(root.path()).is_empty());
    }

    #[test]
    fn torn_last_record_is_cut() {
        let root = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::create(root.path(), &json!({}), Limits::default()).unwrap();
        w.write_line("{\"ok\":1}").unwrap();
        w.out.write_all(b"{\"torn\":").unwrap();
        w.flush().unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);
        assert_eq!(unpack(&bundle(&dir, 1 << 20).unwrap().gz), "{}\n{\"ok\":1}\n");
    }

    #[test]
    fn pruning_spares_foreign_directories_and_files() {
        let root = tempfile::tempdir().unwrap();
        let foreign = root.path().join("journal-notes");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("keep.txt"), "x").unwrap();
        let w = SessionWriter::create(root.path(), &json!({"k": 1}), small()).unwrap();
        let dir = w.dir().to_path_buf();
        fs::write(dir.join("operator.txt"), "x").unwrap();
        w.close().unwrap();
        prune_sessions(root.path(), 0);
        assert!(foreign.join("keep.txt").exists());
        assert!(dir.join("operator.txt").exists());
        assert!(!dir.join(HEADER).exists());
    }
}
