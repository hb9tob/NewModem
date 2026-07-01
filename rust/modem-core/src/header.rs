//! Profile-index derivation + the `Header` result DTO.
//!
//! V4 dropped the on-air protocol header: its only live field, `profile_index`,
//! moved into the bootstrap **marker** (see `marker.rs` /
//! `V4_WIRE_FORMAT_PLAN.md`). The former wire-header codec — Golay(24,12) →
//! 96 QPSK symbols, the 12-byte magic+CRC serialisation, the freq-offset
//! packing — is therefore gone. What remains is:
//!
//!   * [`derive_profile_index`] — reused by the TX (`frame.rs`) to stamp the
//!     marker's `profile_index`;
//!   * [`Header`] — a lightweight struct the RX *synthesises* from the decoded
//!     bootstrap marker (+ active config) and surfaces as
//!     [`crate::rx_v2::RxV2Result::header`], so the worker keeps its existing
//!     `result.header.{profile_index,mode_code,version,…}` access paths.

use crate::profile::ModemConfig;

/// End-of-transmission flag. V4 carries EOT in the bootstrap marker's
/// `EOT_FRAME_BIT`; the RX mirrors it here in the synthesised `Header.flags`
/// so consumers that test the header flag keep working.
pub const FLAG_EOT: u8 = 0x04;

/// Decoded header information surfaced in [`crate::rx_v2::RxV2Result`].
///
/// In V4 there is no on-air header to parse — this is built from the bootstrap
/// marker (`profile_index`, EOT flag) plus the active config. Fields that the
/// dropped wire header used to carry (`frame_counter`, `freq_offset`, and a
/// real `payload_length`) are retained for source compatibility and left at
/// their defaults.
#[derive(Clone, Debug)]
pub struct Header {
    pub version: u8,
    pub mode_code: u8,
    pub frame_counter: u16,
    pub payload_length: u16,
    pub flags: u8,
    pub freq_offset: u8,
    /// Canonical `ProfileIndex::as_u8()`, or `ProfileIndex::UNKNOWN` (0xFF)
    /// when the config matches no predefined profile.
    pub profile_index: u8,
}

/// Best-effort mapping from `ModemConfig` to a canonical profile byte, by
/// matching the tuple `(constellation, ldpc_rate, symbol_rate, tau)` against
/// the predefined profiles. Returns `ProfileIndex::UNKNOWN` if nothing matches
/// (custom rate combo).
///
/// V4: used by `frame.rs` to stamp `profile_index` into the bootstrap marker.
pub(crate) fn derive_profile_index(config: &ModemConfig) -> u8 {
    use crate::profile::ProfileIndex;
    for p in ProfileIndex::ALL {
        let ref_cfg = p.to_config();
        if ref_cfg.constellation == config.constellation
            && ref_cfg.ldpc_rate == config.ldpc_rate
            && (ref_cfg.symbol_rate - config.symbol_rate).abs() < 1.0
            && (ref_cfg.tau - config.tau).abs() < 1e-6
        {
            return p.as_u8();
        }
    }
    ProfileIndex::UNKNOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_profile_index_matches_all_profiles() {
        use crate::profile::{
            profile_high, profile_normal, profile_robust, profile_ultra, ProfileIndex,
        };
        // MEGA retiré 2026-06-30 — hors registre, derive_profile_index le
        // mappe désormais sur UNKNOWN (comportement attendu d'un profil retiré).
        let cases: [(ModemConfig, ProfileIndex); 4] = [
            (profile_ultra(), ProfileIndex::Ultra),
            (profile_robust(), ProfileIndex::Robust),
            (profile_normal(), ProfileIndex::Normal),
            (profile_high(), ProfileIndex::High),
        ];
        for (cfg, expected) in cases {
            assert_eq!(
                derive_profile_index(&cfg),
                expected.as_u8(),
                "derive_profile_index mismatch for {expected:?}"
            );
        }
    }
}
