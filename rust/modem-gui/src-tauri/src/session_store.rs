//! Disk-persistent RX session store.
//!
//! Each session (= unique `session_id`) owns a folder under
//! `<save_dir>/sessions/<session_id>.session/`. Its content is :
//!
//! - `meta.json` : session metadata + decoder OTI (serializable).
//! - `packets.blob` : append-only stream of `(u32 LE ESI)(T bytes payload)`.
//! - `decoded.<ext>` : the decoded payload once RaptorQ converges (optional).
//!
//! The RX worker feeds every freshly validated codeword into the store via
//! `SessionStore::accept_packet`. The store deduplicates by ESI, appends to
//! the blob, and — whenever the RaptorQ decoder has enough packets — writes
//! the decoded file and emits a `DecodedOutcome`. Everything is crash-safe :
//! restart the app, the session picks up exactly where it left off.
//!
//! # Lifecycle
//! - `SessionStore::new(dir)` scans the directory and drops sessions older
//!   than 24 h (`MAX_AGE_HOURS`).
//! - `accept_packet` lazily loads a session on first seen `session_id`.
//! - Session is never "closed" — only removed by the user (rm -rf) or by the
//!   24 h cleanup on next boot.

use modem_core::app_header::AppHeader;
use modem_core::profile::ProfileIndex;
use modem_core::raptorq_codec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Age at which a session folder is dropped on next `SessionStore::new`.
const MAX_AGE_HOURS: u64 = 24;

/// Hard cap on the packet blob : 3 × file_size. Beyond this, packets are
/// discarded — the fountain has failed on this channel, keep 3× as a
/// diagnostic bound.
const BLOB_CAP_RATIO: u32 = 3;

/// Warning ratio : the UI should flag a degraded channel at or above this.
pub const BLOB_WARN_RATIO: u32 = 2;

/// On-disk session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: u32,
    pub k_symbols: u16,
    pub t_bytes: u8,
    pub file_size: u32,
    pub mode_code: u8,
    pub mime_type: u8,
    pub hash_short: u16,
    pub profile: String,
    /// Seconds since UNIX epoch, set at session creation.
    pub created_at: u64,
    /// Filled once RaptorQ decoded successfully and the envelope was read.
    #[serde(default)]
    pub callsign: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub decoded: bool,
}

impl SessionMeta {
    fn from_app_header(ah: &AppHeader, profile: ProfileIndex) -> Self {
        let created_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            session_id: ah.session_id,
            k_symbols: ah.k_symbols,
            t_bytes: ah.t_bytes,
            file_size: ah.file_size,
            mode_code: ah.mode_code,
            mime_type: ah.mime_type,
            hash_short: ah.hash_short,
            profile: format!("{profile:?}"),
            created_at,
            callsign: None,
            filename: None,
            decoded: false,
        }
    }
}

/// In-memory state of a single session. Packets and decoder live entirely
/// on disk ; this struct just caches the dedup set and the meta.
struct SessionState {
    meta: SessionMeta,
    dir: PathBuf,
    /// ESIs already present on disk, for cheap dedup.
    seen: HashSet<u32>,
    blob_path: PathBuf,
    meta_path: PathBuf,
}

impl SessionState {
    fn blob_cap_bytes(&self) -> u64 {
        let file = self.meta.file_size as u64;
        let t = (self.meta.t_bytes as u64 + 4) * BLOB_CAP_RATIO as u64;
        file * t / self.meta.t_bytes.max(1) as u64
    }

    fn load_seen(&mut self) {
        self.seen.clear();
        if !self.blob_path.exists() {
            return;
        }
        let Ok(mut f) = File::open(&self.blob_path) else { return; };
        let entry_size = 4 + self.meta.t_bytes as usize;
        let mut buf = vec![0u8; entry_size];
        while f.read_exact(&mut buf).is_ok() {
            let esi = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            self.seen.insert(esi);
        }
    }

    fn append_packet(&mut self, esi: u32, payload: &[u8]) -> std::io::Result<()> {
        debug_assert_eq!(payload.len(), self.meta.t_bytes as usize);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.blob_path)?;
        f.write_all(&esi.to_le_bytes())?;
        f.write_all(payload)?;
        self.seen.insert(esi);
        Ok(())
    }

    /// Load all (esi, bytes) pairs from the blob on disk. Called whenever
    /// we want to try a fountain decode.
    fn read_all_packets(&self) -> HashMap<u32, Vec<u8>> {
        let mut out = HashMap::new();
        let Ok(mut f) = File::open(&self.blob_path) else { return out; };
        let t = self.meta.t_bytes as usize;
        let mut header = [0u8; 4];
        let mut payload = vec![0u8; t];
        while f.read_exact(&mut header).is_ok() {
            if f.read_exact(&mut payload).is_err() {
                break;
            }
            let esi = u32::from_le_bytes(header);
            out.entry(esi).or_insert_with(|| payload.clone());
        }
        out
    }

    fn persist_meta(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&self.meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&self.meta_path, json)
    }

    fn blob_size(&self) -> u64 {
        fs::metadata(&self.blob_path)
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

/// Outcome of feeding a packet to a session.
#[derive(Debug, Clone)]
pub struct AcceptOutcome {
    /// True if this was a new ESI (actually appended to the blob).
    pub accepted: bool,
    /// Current number of unique ESIs stored.
    pub unique_esis: u32,
    /// RaptorQ K (convergence threshold).
    pub needed: u32,
    /// Whether the blob reached the hard cap and further packets will drop.
    pub cap_reached: bool,
    /// Decoded file (if RaptorQ converged on this packet).
    pub decoded: Option<DecodedFile>,
    /// Cumulative bitmap over the K source ESIs : bit i = 1 iff ESI i has
    /// been received at least once since this session was created. Covers
    /// repair ESIs (index ≥ K) only up to the K-th bit — repair packets
    /// contribute to the fountain decode but aren't part of the UI bar.
    pub seen_bitmap: Vec<u8>,
}

/// Successfully decoded file, written on disk inside the session dir.
#[derive(Debug, Clone)]
pub struct DecodedFile {
    pub session_id: u32,
    pub session_dir: PathBuf,
    pub decoded_path: PathBuf,
    pub payload: Vec<u8>,
    pub meta: SessionMeta,
}

/// The disk-backed session registry.
pub struct SessionStore {
    root: PathBuf,
    /// In-memory cache keyed by session_id. Loaded lazily.
    sessions: HashMap<u32, SessionState>,
}

impl SessionStore {
    /// Create or open a store under `<save_dir>/sessions/`. Runs the 24 h
    /// cleanup immediately so stale sessions don't accumulate.
    pub fn new(save_dir: &Path) -> std::io::Result<Self> {
        let root = save_dir.join("sessions");
        fs::create_dir_all(&root)?;
        let mut store = Self {
            root,
            sessions: HashMap::new(),
        };
        store.cleanup_expired();
        Ok(store)
    }

    /// Delete every `*.session/` whose folder mtime is older than 24 hours.
    /// Errors are swallowed (logged via eprintln) so a permission issue on
    /// one folder doesn't break the boot.
    pub fn cleanup_expired(&self) {
        self.cleanup_older_than(Duration::from_secs(MAX_AGE_HOURS * 3600));
    }

    /// Internal : delete every `*.session/` older than `max_age`. Exposed
    /// for tests that want to force a cleanup without waiting 24 h.
    pub fn cleanup_older_than(&self, max_age: Duration) {
        let Ok(entries) = fs::read_dir(&self.root) else { return; };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
            if !name.ends_with(".session") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue; };
            let Ok(modified) = meta.modified() else { continue; };
            let Ok(age) = SystemTime::now().duration_since(modified) else { continue; };
            if age > max_age {
                if let Err(e) = fs::remove_dir_all(&path) {
                    eprintln!("[session_store] cleanup rm {path:?} failed: {e}");
                }
            }
        }
    }

    fn session_dir_for(&self, session_id: u32) -> PathBuf {
        self.root.join(format!("{session_id:08x}.session"))
    }

    /// Create the session folder + meta.json if they don't exist ; load the
    /// state into the in-memory cache. Idempotent.
    fn ensure_session(&mut self, ah: &AppHeader, profile: ProfileIndex) {
        if self.sessions.contains_key(&ah.session_id) {
            return;
        }
        let dir = self.session_dir_for(ah.session_id);
        let _ = fs::create_dir_all(&dir);
        let meta_path = dir.join("meta.json");
        let meta = if meta_path.exists() {
            fs::read_to_string(&meta_path)
                .ok()
                .and_then(|s| serde_json::from_str::<SessionMeta>(&s).ok())
                .unwrap_or_else(|| SessionMeta::from_app_header(ah, profile))
        } else {
            let m = SessionMeta::from_app_header(ah, profile);
            if let Ok(j) = serde_json::to_string_pretty(&m) {
                let _ = fs::write(&meta_path, j);
            }
            m
        };
        let blob_path = dir.join("packets.blob");
        let mut state = SessionState {
            meta,
            dir,
            seen: HashSet::new(),
            blob_path,
            meta_path,
        };
        state.load_seen();
        self.sessions.insert(ah.session_id, state);
    }

    /// Feed a batch of (ESI, T-byte) packets for a session. Returns the
    /// outcome : number of new ESIs stored, whether the decoder converged,
    /// and the decoded file if any.
    ///
    /// Assumes the AppHeader has already been received ; callers must not
    /// invoke this before they have an AppHeader for the session.
    pub fn accept_packets(
        &mut self,
        ah: &AppHeader,
        profile: ProfileIndex,
        packets: &HashMap<u32, Vec<u8>>,
    ) -> AcceptOutcome {
        self.ensure_session(ah, profile);
        let state = self
            .sessions
            .get_mut(&ah.session_id)
            .expect("ensured above");

        let cap = state.blob_cap_bytes();
        let mut accepted = 0u32;
        let mut cap_reached = false;
        for (&esi, bytes) in packets.iter() {
            if bytes.len() != state.meta.t_bytes as usize {
                // Ignore mismatches (shouldn't happen — rx_v3 CW size = T).
                continue;
            }
            if state.seen.contains(&esi) {
                continue;
            }
            if state.blob_size() >= cap {
                cap_reached = true;
                break;
            }
            if let Err(e) = state.append_packet(esi, bytes) {
                eprintln!("[session_store] append failed: {e}");
                continue;
            }
            accepted += 1;
        }

        let unique_esis = state.seen.len() as u32;
        let needed = state.meta.k_symbols as u32;
        let seen_bitmap = build_seen_bitmap(&state.seen, needed);
        let mut outcome = AcceptOutcome {
            accepted: accepted > 0,
            unique_esis,
            needed,
            cap_reached,
            decoded: None,
            seen_bitmap,
        };

        // Attempt a fountain decode every time we added at least one packet
        // and we have ≥ K unique ESIs.
        if accepted > 0 && unique_esis >= needed && !state.meta.decoded {
            let collected = state.read_all_packets();
            if let Some(payload) = raptorq_codec::try_decode(
                &collected,
                state.meta.file_size,
                state.meta.t_bytes as u16,
            ) {
                let ext = mime_ext(state.meta.mime_type);
                let decoded_path = state.dir.join(format!("decoded.{ext}"));
                if let Err(e) = fs::write(&decoded_path, &payload) {
                    eprintln!("[session_store] decoded write: {e}");
                } else {
                    state.meta.decoded = true;
                    // Best-effort meta enrichment with envelope fields.
                    let env = modem_core::payload_envelope::PayloadEnvelope::decode_or_fallback(&payload);
                    if env.version != 0 {
                        state.meta.callsign = Some(env.callsign.clone());
                        state.meta.filename = Some(env.filename.clone());
                    }
                    let _ = state.persist_meta();
                }
                outcome.decoded = Some(DecodedFile {
                    session_id: ah.session_id,
                    session_dir: state.dir.clone(),
                    decoded_path,
                    payload,
                    meta: state.meta.clone(),
                });
            }
        }

        outcome
    }

    /// If the session `session_id` is already decoded on disk, load the
    /// decoded payload + meta from disk and return it. Returns `None` if the
    /// session doesn't exist, isn't decoded yet, or the decoded file is
    /// missing from disk.
    ///
    /// Used to re-emit `session_decoded` / `file_complete` when a fresh
    /// capture episode starts over a session that was already completed in a
    /// previous episode (e.g. same file re-transmitted by the operator).
    pub fn peek_decoded(&mut self, ah: &AppHeader, profile: ProfileIndex) -> Option<DecodedFile> {
        self.ensure_session(ah, profile);
        let state = self.sessions.get(&ah.session_id)?;
        if !state.meta.decoded {
            return None;
        }
        let ext = mime_ext(state.meta.mime_type);
        let decoded_path = state.dir.join(format!("decoded.{ext}"));
        let payload = fs::read(&decoded_path).ok()?;
        Some(DecodedFile {
            session_id: state.meta.session_id,
            session_dir: state.dir.clone(),
            decoded_path,
            payload,
            meta: state.meta.clone(),
        })
    }

    /// Read all sessions currently on disk (for UI listing / debug).
    pub fn list_all(&self) -> Vec<SessionMeta> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else { return out; };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta_path = path.join("meta.json");
            if let Ok(s) = fs::read_to_string(&meta_path) {
                if let Ok(m) = serde_json::from_str::<SessionMeta>(&s) {
                    out.push(m);
                }
            }
        }
        out
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Pack the K-bit "source ESI seen" bitmap from the session's seen set.
fn build_seen_bitmap(seen: &HashSet<u32>, k: u32) -> Vec<u8> {
    if k == 0 {
        return Vec::new();
    }
    let bytes = ((k + 7) / 8) as usize;
    let mut out = vec![0u8; bytes];
    for &esi in seen {
        if esi < k {
            let i = esi as usize;
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

/// Map a MIME byte to a file extension for the decoded file.
fn mime_ext(mime: u8) -> &'static str {
    use modem_core::app_header::mime;
    match mime {
        mime::TEXT => "txt",
        mime::IMAGE_AVIF => "avif",
        mime::IMAGE_JPEG => "jpg",
        mime::IMAGE_PNG => "png",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_core::app_header::mime;
    use modem_core::raptorq_codec;

    fn tmp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nbfm_session_test_{name}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_ah(file_size: u32, k: u16, t: u8) -> AppHeader {
        AppHeader {
            session_id: 0xABCD_1234,
            file_size,
            k_symbols: k,
            t_bytes: t,
            mode_code: 0xA5,
            mime_type: mime::BINARY,
            hash_short: 0x1111,
        }
    }

    #[test]
    fn accept_dedup_and_decode() {
        let save = tmp_dir("accept_decode");
        let mut store = SessionStore::new(&save).unwrap();

        let t: u16 = 16;
        let data: Vec<u8> = (0..100).map(|i| (i as u8).wrapping_mul(7)).collect();
        let packets = raptorq_codec::encode_packets(&data, t, 30);
        let k = raptorq_codec::k_from_payload(data.len(), t as usize) as u32;
        let ah = sample_ah(data.len() as u32, k as u16, t as u8);

        // Feed only K packets : should decode.
        let mut map = HashMap::new();
        for (i, p) in packets.iter().take(k as usize).enumerate() {
            map.insert(i as u32, p.clone());
        }
        let outcome = store.accept_packets(&ah, ProfileIndex::High, &map);
        assert!(outcome.accepted);
        assert_eq!(outcome.unique_esis, k);
        let decoded = outcome.decoded.expect("must decode at K packets");
        assert_eq!(&decoded.payload[..data.len()], &data[..]);
        assert!(decoded.decoded_path.exists());

        // Re-feed the same packets : dedup → no new ESIs.
        let outcome2 = store.accept_packets(&ah, ProfileIndex::High, &map);
        assert!(!outcome2.accepted);
        assert_eq!(outcome2.unique_esis, k);
    }

    #[test]
    fn blob_persists_across_reload() {
        let save = tmp_dir("reload");
        let t: u16 = 16;
        let data: Vec<u8> = (0..100).map(|i| (i * 17) as u8).collect();
        let packets = raptorq_codec::encode_packets(&data, t, 30);
        let k = raptorq_codec::k_from_payload(data.len(), t as usize) as u32;
        let ah = sample_ah(data.len() as u32, k as u16, t as u8);

        // First boot : feed K/2 packets.
        {
            let mut store = SessionStore::new(&save).unwrap();
            let mut map = HashMap::new();
            for (i, p) in packets.iter().take((k / 2) as usize).enumerate() {
                map.insert(i as u32, p.clone());
            }
            let outcome = store.accept_packets(&ah, ProfileIndex::High, &map);
            assert!(outcome.decoded.is_none());
        }

        // Second boot : feed the remaining K/2+ packets, must decode.
        {
            let mut store = SessionStore::new(&save).unwrap();
            let mut map = HashMap::new();
            for (i, p) in packets.iter().enumerate().skip((k / 2) as usize) {
                map.insert(i as u32, p.clone());
            }
            let outcome = store.accept_packets(&ah, ProfileIndex::High, &map);
            assert!(outcome.decoded.is_some(), "must decode after persistence reload");
        }
    }

    #[test]
    fn peek_decoded_returns_stored_payload_for_decoded_session() {
        let save = tmp_dir("peek");
        let t: u16 = 16;
        let data: Vec<u8> = (0..80).map(|i| (i as u8).wrapping_mul(11)).collect();
        let packets = raptorq_codec::encode_packets(&data, t, 30);
        let k = raptorq_codec::k_from_payload(data.len(), t as usize) as u32;
        let ah = sample_ah(data.len() as u32, k as u16, t as u8);

        // First : feed K packets, session decodes.
        {
            let mut store = SessionStore::new(&save).unwrap();
            let mut map = HashMap::new();
            for (i, p) in packets.iter().take(k as usize).enumerate() {
                map.insert(i as u32, p.clone());
            }
            let outcome = store.accept_packets(&ah, ProfileIndex::High, &map);
            assert!(outcome.decoded.is_some(), "must decode at K packets");
        }

        // Second : a fresh store over the same folder finds the decoded
        // session on disk ; peek_decoded must return it without requiring
        // any new packets.
        {
            let mut store = SessionStore::new(&save).unwrap();
            let df = store.peek_decoded(&ah, ProfileIndex::High).expect("must peek");
            assert_eq!(df.session_id, ah.session_id);
            assert_eq!(&df.payload[..data.len()], &data[..]);
            assert!(df.meta.decoded);
        }
    }

    #[test]
    fn peek_decoded_returns_none_before_decode() {
        let save = tmp_dir("peek_pending");
        let t: u16 = 16;
        let data: Vec<u8> = (0..80).map(|i| (i as u8).wrapping_mul(9)).collect();
        let packets = raptorq_codec::encode_packets(&data, t, 30);
        let k = raptorq_codec::k_from_payload(data.len(), t as usize) as u32;
        let ah = sample_ah(data.len() as u32, k as u16, t as u8);
        let mut store = SessionStore::new(&save).unwrap();
        // Feed only K/2 packets — not enough to decode.
        let mut map = HashMap::new();
        for (i, p) in packets.iter().take((k / 2) as usize).enumerate() {
            map.insert(i as u32, p.clone());
        }
        let outcome = store.accept_packets(&ah, ProfileIndex::High, &map);
        assert!(outcome.decoded.is_none());
        assert!(store.peek_decoded(&ah, ProfileIndex::High).is_none());
    }

    #[test]
    fn cleanup_removes_session_older_than_cutoff() {
        let save = tmp_dir("cleanup");
        let store = SessionStore::new(&save).unwrap();
        let dir = store.session_dir_for(0xDEADBEEF);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("packets.blob"), b"dummy").unwrap();
        assert!(dir.exists());

        // Force a micro-cleanup : "anything older than 0 ns" — sleeps by 1 ms
        // first so the folder mtime is strictly in the past.
        std::thread::sleep(Duration::from_millis(10));
        store.cleanup_older_than(Duration::from_millis(1));
        assert!(!dir.exists(), "session folder should have been removed");
    }

    #[test]
    fn cleanup_keeps_fresh_session() {
        let save = tmp_dir("cleanup_keep");
        let store = SessionStore::new(&save).unwrap();
        let dir = store.session_dir_for(0xFEED_CAFE);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("packets.blob"), b"dummy").unwrap();

        // Default 24 h cutoff must leave the freshly-created folder alone.
        store.cleanup_expired();
        assert!(dir.exists(), "fresh session must survive default cleanup");
    }
}
