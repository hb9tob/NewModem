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
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::Value;

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

/// The most recent session that is no longer running, sent or not.
pub fn previous(root: &Path) -> Option<PathBuf> {
    sessions(root).into_iter().rev().find(|d| !is_live(d))
}

/// What the operator reads before publishing: the header and the last
/// `max` bytes of records, whole lines only.
pub fn preview(dir: &Path, max: u64) -> io::Result<String> {
    let mut text = String::from_utf8_lossy(&fs::read(dir.join(HEADER))?).into_owned();
    let mut tail: Vec<Vec<u8>> = Vec::new();
    let mut left = max;
    let mut cut = false;
    for seg in segments(dir).iter().rev() {
        let mut file = File::open(seg)?;
        let len = file.metadata()?.len();
        let take = len.min(left);
        cut |= take < len;
        file.seek(SeekFrom::Start(len - take))?;
        let mut raw = Vec::new();
        file.take(take).read_to_end(&mut raw)?;
        if take < len {
            let first = raw.iter().position(|&b| b == b'\n').map_or(raw.len(), |i| i + 1);
            raw.drain(..first);
        }
        left -= take;
        tail.push(raw);
    }
    if cut {
        text.push_str("...\n");
    }
    for raw in tail.iter().rev() {
        text.push_str(&String::from_utf8_lossy(raw));
    }
    Ok(text)
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

/// Queue level counted as an incident; default of `analyze_modem_journal.py`.
const INCIDENT_QUEUE_MS: f64 = 500.0;
/// Same set as `INCIDENT_KINDS` in that script.
const INCIDENT_KINDS: [&str; 8] = [
    "rx.brickwall",
    "rx.session_lost",
    "rx.driver_drop",
    "error",
    "panic",
    "tx_error",
    "worker_requests_restart",
    "truncated",
];

/// What the public index of the collector shows about a journal.
#[derive(Debug, Default)]
pub struct Summary {
    pub version: Option<String>,
    pub host: Option<String>,
    pub duration_s: f64,
    pub queue_max_ms: f64,
    pub incidents: u32,
    /// Neither closed nor still running.
    pub crashed: bool,
}

/// A bundle and its summary, computed on what is really sent. Reads and
/// compresses the whole session: not for an async or GUI thread.
pub fn submission(dir: &Path, max_gz: usize) -> io::Result<(Bundle, Summary)> {
    let bundle = bundle(dir, max_gz)?;
    let mut out = Summary {
        crashed: !dir.join(CLOSED).exists() && !is_live(dir),
        ..Default::default()
    };
    let mut queue_high = false;
    let lines = BufReader::new(MultiGzDecoder::new(&bundle.gz[..])).split(b'\n');
    for (index, line) in lines.enumerate() {
        let Ok(record) = serde_json::from_slice::<Value>(&line?) else {
            continue;
        };
        if let Some(t) = record["t_ms"].as_f64() {
            out.duration_s = out.duration_s.max(t / 1000.0);
        }
        match record["kind"].as_str() {
            // The bundle starts with the session header.
            Some("header") if index == 0 => {
                out.version = record["version"].as_str().map(super::sanitize::text);
                out.host = host_line(&record["host"]);
            }
            Some("buffer") => {
                let queue = record["queue_max_ms"].as_f64().unwrap_or(0.0);
                out.queue_max_ms = out.queue_max_ms.max(queue);
                let high = queue >= INCIDENT_QUEUE_MS;
                // The crossing, not every second spent above.
                out.incidents += u32::from(high && !queue_high);
                queue_high = high;
            }
            Some(kind) if INCIDENT_KINDS.contains(&kind) => out.incidents += 1,
            _ => {}
        }
    }
    Ok((bundle, out))
}

fn host_line(host: &Value) -> Option<String> {
    let mut parts: Vec<String> = ["os", "arch", "cpu"]
        .iter()
        .filter_map(|key| host[*key].as_str())
        .map(super::sanitize::text)
        .collect();
    if let Some(cores) = host["cores"].as_u64() {
        parts.push(format!("{cores} cores"));
    }
    if let Some(ram) = host["ram_mib"].as_u64() {
        parts.push(format!("{ram} MiB"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

// Default level: measured 7.7x on RX journals for 0.06 CPU s/MB. Level 9
// gains 4 % for almost twice the CPU.
fn gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn submission_summarises_what_is_sent() {
        let root = tempfile::tempdir().unwrap();
        let header = json!({"kind": "header", "version": "1.2", "host": {"os": "linux", "cores": 4}});
        let mut w = SessionWriter::create(root.path(), &header, Limits::default()).unwrap();
        for (t, queue) in [(1000, 20), (2000, 700), (3000, 900), (4000, 10), (5000, 500)] {
            w.write_line(&json!({"kind": "buffer", "t_ms": t, "queue_max_ms": queue}).to_string())
                .unwrap();
        }
        w.write_line("{\"kind\":\"tx_error\",\"t_ms\":6000}").unwrap();
        w.write_line("{\"kind\":\"header\",\"t_ms\":6000}").unwrap();
        w.flush().unwrap();
        let dir = w.dir().to_path_buf();
        assert!(!submission(&dir, 1 << 20).unwrap().1.crashed, "live session reported as crashed");

        drop(w);
        let (_, summary) = submission(&dir, 1 << 20).unwrap();
        assert_eq!(summary.version.as_deref(), Some("1.2"));
        assert_eq!(summary.host.as_deref(), Some("linux, 4 cores"));
        assert_eq!((summary.duration_s, summary.queue_max_ms), (6.0, 900.0));
        assert_eq!(summary.incidents, 3);
        assert!(summary.crashed);
    }

    #[test]
    fn preview_keeps_header_and_whole_tail_lines() {
        let root = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::create(root.path(), &json!({"h": 1}), small()).unwrap();
        for i in 0..150 {
            w.write_line(&format!("{{\"i\":{i}}}")).unwrap();
        }
        w.flush().unwrap();
        let dir = w.dir().to_path_buf();
        let all = preview(&dir, 1 << 20).unwrap();
        assert!(all.contains("{\"i\":0}") && !all.contains("..."));

        let tail = preview(&dir, 100).unwrap();
        let mut lines = tail.lines();
        assert_eq!(lines.next(), Some("{\"h\":1}"));
        assert_eq!(lines.next(), Some("..."));
        assert!(lines.clone().all(|l| l.starts_with("{\"i\":") && l.ends_with('}')));
        assert_eq!(lines.last(), Some("{\"i\":149}"));
    }

    #[test]
    fn previous_is_the_newest_dead_session() {
        let root = tempfile::tempdir().unwrap();
        let w = SessionWriter::create(root.path(), &json!({}), Limits::default()).unwrap();
        assert!(previous(root.path()).is_none(), "live session returned");
        let dir = w.dir().to_path_buf();
        w.close().unwrap();
        assert_eq!(previous(root.path()), Some(dir));
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
