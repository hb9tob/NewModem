//! Cross-cutting abstractions for swappable modem implementations.
//!
//! The `Modem` trait is the contract upper layers (worker, framing, GUI) use
//! to discover what modes a given physical-layer implementation supports and
//! their headline capabilities. A future OFDM or QO-100 modem can implement
//! the same trait and slot in without the GUI knowing the difference.
//!
//! Phase 1 surface: capabilities only. Encode/decode are added in phase 2
//! when the worker is extracted and starts calling the trait.

use serde::{Deserialize, Serialize};

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

/// Trait every modem implementation exposes to the upper layers.
///
/// Phase 1 keeps the surface to capability discovery. Encode/decode
/// methods will be added in phase 2 alongside the worker extraction.
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
}
