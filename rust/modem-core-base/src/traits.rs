//! Cross-cutting abstractions for swappable modem implementations.
//!
//! The `Modem` trait is the contract upper layers (worker, framing, GUI) use
//! to discover what modes a given physical-layer implementation supports and
//! their headline capabilities. A future OFDM or QO-100 modem can implement
//! the same trait and slot in without the GUI knowing the difference.
//!
//! Phase 1 surface: capabilities only.
//! Phase 2D adds the TX-side `encode_to_samples` so the GUI worker can
//! emit audio without spawning the legacy CLI subprocess.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Public-facing description of a single modem profile/mode.
///
/// Returned by `Modem::list_profiles()` so upper layers can populate UI
/// combos, compute estimates, and label sessions without hard-coding a
/// list of mode names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileDescriptor {
    /// Stable identifier (e.g. "HIGH56", "ULTRA"). Used as the key on the
    /// wire (header `profile_index` stays internal) and as the value in
    /// the GUI selector.
    pub name: String,

    /// Family identifier (e.g. "NBFM-V3"). Lets the UI group profiles
    /// when more than one modem implementation is registered.
    pub family: String,

    /// Human-friendly label suitable for a combo entry, e.g.
    /// "HIGH56 — 16-APSK 5/6, 4706 bps".
    pub label: String,

    /// Net bitrate after FEC + pilot overhead (bits/s).
    pub bitrate_bps: f64,

    /// Bits per modulated symbol (2 for QPSK, 4 for 16-APSK, ...).
    pub bits_per_symbol: u32,

    /// Symbol rate in baud.
    pub symbol_rate_bd: f64,

    /// LDPC code rate (0.5, 0.75, 5/6, ...).
    pub ldpc_rate: f64,

    /// Profile is experimental and outside the auto-detection gate. The
    /// GUI displays it with a warning badge and only when the user has
    /// enabled experimental modes.
    pub experimental: bool,
}

/// Single TX request — everything an implementation needs to turn an
/// already-framed wire payload into transmittable audio samples.
///
/// `wire_payload` is what the framing layer (RaptorQ + envelope + headers
/// in V3) produced; `n_packets` is how many RaptorQ-style packets the
/// modem should encode (K source + repair, rounded up to a complete
/// segment by the framing layer's helper). `esi_start` lets the worker
/// emit a continuation burst (ESI offset) without rewinding the session.
///
/// `vox_seconds` = duration of the carrier-frequency VOX preamble emitted
/// before the data frame. 0.0 = no VOX (the GUI worker, which uses PTT
/// for switching, can choose this; the legacy CLI defaults to 0.5).
#[derive(Clone, Debug)]
pub struct EncodeRequest<'a> {
    pub profile: &'a str,
    pub wire_payload: &'a [u8],
    pub session_id: u32,
    pub mime_type: u8,
    pub hash_short: u16,
    pub esi_start: u32,
    pub n_packets: u32,
    pub vox_seconds: f64,
}

/// Errors a `Modem` implementation can surface to its caller.
#[derive(Debug, Error)]
pub enum ModemError {
    #[error("unknown profile: {0}")]
    UnknownProfile(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

/// Trait every modem implementation exposes to the upper layers.
pub trait Modem: Send + Sync {
    /// Family identifier shared by every profile this implementation
    /// owns (e.g. "NBFM-V3").
    fn family(&self) -> &'static str;

    /// All profiles this modem can encode and decode, in canonical
    /// order. Includes experimental ones (the GUI filters them).
    fn list_profiles(&self) -> Vec<ProfileDescriptor>;

    /// Look up a profile by its stable name. Returns `None` if the name
    /// is unknown to this implementation.
    fn profile_by_name(&self, name: &str) -> Option<ProfileDescriptor>;

    /// Encode an already-framed wire payload into mono 48 kHz f32 audio
    /// samples ready for a sound-card / SDR sink. The output includes
    /// every protocol-level appendage the modem needs (preamble,
    /// markers, EOT frame, inter-frame silence, etc.) and is byte-for-
    /// byte equivalent to what the legacy `nbfm-modem tx` CLI used to
    /// write into a WAV — verified by the integration test in
    /// `modem-worker/tests/cli_parity.rs`.
    fn encode_to_samples(&self, req: &EncodeRequest<'_>) -> Result<Vec<f32>, ModemError>;
}
