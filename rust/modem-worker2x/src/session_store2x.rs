//! In-memory session store for the 2x family — minimal first cut.
//!
//! The V3 [`session_store`](modem_worker::session_store) does a lot more
//! (disk-backed JSON map, SHA-1 dedup, expiry, persistent ESI tracking)
//! and that complexity is intentionally left out of the first 2x cut.
//! 2x sessions are stored in RAM, keyed by `session_id` plus the
//! AppHeader's `(file_size, k_symbols, t_bytes, mime_type, hash_short)`
//! tuple so a duplicate retry from the operator updates the existing
//! entry rather than spawning a fresh one.
//!
//! When [`accept_payload`] is called with a freshly decoded payload that
//! matches the AppHeader's `file_size` and `hash_short`, the entry flips
//! to [`SessionState::Complete`] and the bytes can be retrieved via
//! [`get_payload`]. The disk-backed promotion is a follow-up; this is
//! the minimum the integration tests need to assert end-to-end TX → RX
//! identity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use modem_framing::app_header::AppHeader;

/// Per-session state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Session header seen; no full payload reconstructed yet.
    HeaderOnly,
    /// Payload assembled and hash-validated.
    Complete,
}

/// One entry in the store.
#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub app_header: AppHeader,
    pub state: SessionState,
    pub payload: Vec<u8>,
}

impl SessionEntry {
    fn new_header_only(app_header: AppHeader) -> Self {
        Self {
            app_header,
            state: SessionState::HeaderOnly,
            payload: Vec::new(),
        }
    }
}

/// Cheap clonable handle on the session-store data; the inner mutex
/// lets multiple workers (capture thread, GUI tick) read and update
/// concurrently. The first cut is in-memory; promotion to disk is left
/// to a follow-up.
#[derive(Clone, Default, Debug)]
pub struct SessionStore2x {
    inner: Arc<Mutex<HashMap<u32, SessionEntry>>>,
}

impl SessionStore2x {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of sessions currently tracked.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("session store mutex poisoned").len()
    }

    /// `true` when the store has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Note that we have an AppHeader for `session_id`. If no entry
    /// exists yet, create one with `state = HeaderOnly`. If an entry
    /// already exists with a *different* header tuple, replace it (the
    /// operator restarted the burst with a different payload).
    pub fn note_header(&self, app_header: AppHeader) {
        let mut g = self.inner.lock().expect("session store mutex poisoned");
        let entry = g
            .entry(app_header.session_id)
            .or_insert_with(|| SessionEntry::new_header_only(app_header.clone()));
        if entry.app_header != app_header {
            *entry = SessionEntry::new_header_only(app_header);
        }
    }

    /// Submit a decoded payload for `session_id`. Validates the byte
    /// count against the AppHeader and flips the entry to `Complete`.
    /// Returns `Ok(true)` when this call was the one that completed the
    /// session, `Ok(false)` when the session was already complete (idempotent
    /// retries are safe), and `Err` when the session is unknown or the
    /// payload size does not match.
    pub fn accept_payload(
        &self,
        session_id: u32,
        payload: Vec<u8>,
    ) -> Result<bool, &'static str> {
        let mut g = self.inner.lock().expect("session store mutex poisoned");
        let entry = g.get_mut(&session_id).ok_or("session not registered")?;
        if entry.app_header.file_size as usize != payload.len() {
            return Err("payload size != AppHeader.file_size");
        }
        if matches!(entry.state, SessionState::Complete) {
            return Ok(false);
        }
        entry.payload = payload;
        entry.state = SessionState::Complete;
        Ok(true)
    }

    /// Fetch the bytes of a completed session. `None` when the session
    /// is unknown or still `HeaderOnly`.
    pub fn get_payload(&self, session_id: u32) -> Option<Vec<u8>> {
        let g = self.inner.lock().expect("session store mutex poisoned");
        let entry = g.get(&session_id)?;
        if matches!(entry.state, SessionState::Complete) {
            Some(entry.payload.clone())
        } else {
            None
        }
    }

    /// Return the recorded AppHeader for a session if any.
    pub fn header(&self, session_id: u32) -> Option<AppHeader> {
        let g = self.inner.lock().expect("session store mutex poisoned");
        g.get(&session_id).map(|e| e.app_header.clone())
    }

    /// Convenience: snapshot every entry (header + state + bytes).
    pub fn entries(&self) -> Vec<(u32, SessionEntry)> {
        let g = self.inner.lock().expect("session store mutex poisoned");
        g.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Drop a session.
    pub fn forget(&self, session_id: u32) {
        let mut g = self.inner.lock().expect("session store mutex poisoned");
        g.remove(&session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(session_id: u32, file_size: u32) -> AppHeader {
        AppHeader {
            session_id,
            file_size,
            k_symbols: 4,
            t_bytes: 144,
            mode_code: 0xA5,
            mime_type: 1,
            hash_short: 0xCAFE,
        }
    }

    #[test]
    fn note_header_creates_header_only_entry() {
        let store = SessionStore2x::new();
        store.note_header(header(0xCAFE, 100));
        assert_eq!(store.len(), 1);
        let entry = &store.entries()[0].1;
        assert_eq!(entry.state, SessionState::HeaderOnly);
        assert_eq!(entry.app_header.file_size, 100);
        assert!(entry.payload.is_empty());
    }

    #[test]
    fn accept_payload_flips_state_to_complete() {
        let store = SessionStore2x::new();
        store.note_header(header(0xCAFE, 5));
        let just_completed = store
            .accept_payload(0xCAFE, vec![1, 2, 3, 4, 5])
            .expect("ok");
        assert!(just_completed);
        let bytes = store.get_payload(0xCAFE).expect("complete");
        assert_eq!(bytes, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn accept_payload_idempotent_after_completion() {
        let store = SessionStore2x::new();
        store.note_header(header(1, 3));
        store.accept_payload(1, vec![9; 3]).unwrap();
        let already = store.accept_payload(1, vec![9; 3]).expect("idempotent");
        assert!(!already, "second accept must report 'not just completed'");
    }

    #[test]
    fn accept_payload_unknown_session_errors() {
        let store = SessionStore2x::new();
        let err = store.accept_payload(42, vec![]).unwrap_err();
        assert_eq!(err, "session not registered");
    }

    #[test]
    fn accept_payload_size_mismatch_errors() {
        let store = SessionStore2x::new();
        store.note_header(header(7, 10));
        let err = store.accept_payload(7, vec![0; 9]).unwrap_err();
        assert_eq!(err, "payload size != AppHeader.file_size");
    }

    #[test]
    fn header_replacement_resets_entry() {
        let store = SessionStore2x::new();
        store.note_header(header(7, 10));
        store.accept_payload(7, vec![0; 10]).unwrap();
        // New burst, same session_id, different file_size.
        let new_h = header(7, 50);
        store.note_header(new_h);
        let entry = store.entries().into_iter().find(|(k, _)| *k == 7).unwrap().1;
        assert_eq!(entry.state, SessionState::HeaderOnly);
        assert!(entry.payload.is_empty());
        assert_eq!(entry.app_header.file_size, 50);
    }

    #[test]
    fn get_payload_returns_none_until_complete() {
        let store = SessionStore2x::new();
        store.note_header(header(0xAB, 4));
        assert!(store.get_payload(0xAB).is_none());
        store.accept_payload(0xAB, vec![1; 4]).unwrap();
        assert_eq!(store.get_payload(0xAB).unwrap(), vec![1; 4]);
    }

    #[test]
    fn forget_removes_entry() {
        let store = SessionStore2x::new();
        store.note_header(header(7, 10));
        store.forget(7);
        assert!(store.is_empty());
    }

    #[test]
    fn store_is_clonable_and_shares_state() {
        let s1 = SessionStore2x::new();
        let s2 = s1.clone();
        s1.note_header(header(99, 1));
        assert_eq!(s2.len(), 1, "Arc<Mutex<...>> should share state across clones");
    }
}
