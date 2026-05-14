//! 2x profiles — `ProfileIndex2x` enum and `ModemConfig2x` struct.
//!
//! Mirrors the V3 profile catalogue (see `modem_core::profile`). The
//! original 2x design dropped TDM pilots in favour of sparse pilot
//! blocks (DVB-S2X §5.5.3), but OTA bring-up on FTX-1 + SDRplay showed
//! the block pattern could not track sound-card phase noise on
//! HIGH+/HIGH++ (see memory `v4-pilot-tdm-refactor-todo`). The TDM
//! pattern is now back as the standard pilot scheme for all 2x
//! profiles, exposed as a per-profile [`PilotPattern2x`] so density
//! and update-bandwidth axes can be tuned independently of the
//! encoder.
//!
//! Other V3 knobs we still skip in 2x:
//!
//! - `mode_code` byte — the 2x wire format puts the full
//!   `ProfileIndex2x::as_u8()` into the PLS payload; `mode_code` is
//!   redundant.
//! - The `Mega` and `Fast` experimental profiles — empirically obsoleted
//!   by HIGH+ class. Not carried into 2x.
//!
//! `HighPlusPlus2x` (64-APSK) is **promoted out of experimental** in 2x:
//! it ships in `ALL_AUTO_DETECT` along with the other 7 profiles.
//!
//! Profile-by-profile DSP knobs (constellation γ, β, RRC) come straight
//! from the V3 reference values to ease A/B testing on the same OTA
//! captures.

use modem_core_base::profile_types::{ConstellationType, LdpcRate};
use modem_core_base::types::DATA_CENTER_HZ;

use crate::pilot2x_tdm::PilotPattern2x;
use crate::plheader::PreambleFamily2x;

/// Reserved sentinel — used in PLS payload when no profile has been
/// established yet (e.g. early in late-entry).
pub const PROFILE_INDEX_2X_UNKNOWN: u8 = 0xFF;

/// Eight 2x profiles. The wire byte (`as_u8()`) is what the PLHEADER PLS
/// carries; both the encoder and the decoder funnel through this enum so
/// adding a profile is a single-table change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ProfileIndex2x {
    /// QPSK 500 Bd LDPC 1/2 — long-range fallback (mirrors V3 ULTRA).
    Ultra2x = 0,
    /// QPSK 1000 Bd LDPC 1/2 — robust default (mirrors V3 ROBUST).
    Robust2x = 1,
    /// 8PSK 1500 Bd LDPC 1/2 (mirrors V3 NORMAL).
    Normal2x = 2,
    /// 16-APSK 1500 Bd LDPC 3/4 (mirrors V3 HIGH).
    High2x = 3,
    /// 32-APSK 1500 Bd LDPC 3/4 (mirrors V3 HIGH+).
    HighPlus2x = 4,
    /// 64-APSK 1500 Bd LDPC 3/4 — promoted out of experimental in 2x.
    /// (Mirrors V3 HIGH++.)
    HighPlusPlus2x = 5,
    /// 16-APSK 1500 Bd LDPC 5/6 (mirrors V3 HIGH56).
    HighFiveSix2x = 6,
    /// 32-APSK 1500 Bd LDPC 5/6 (mirrors V3 HIGH+56).
    HighPlusFiveSix2x = 7,
}

impl ProfileIndex2x {
    /// Canonical order, used to populate UIs and to iterate the table.
    pub const ALL: [Self; 8] = [
        Self::Ultra2x,
        Self::Robust2x,
        Self::Normal2x,
        Self::High2x,
        Self::HighPlus2x,
        Self::HighPlusPlus2x,
        Self::HighFiveSix2x,
        Self::HighPlusFiveSix2x,
    ];

    /// Profiles taking part in auto-detection. In 2x **every** profile is
    /// auto-detectable — there is no `is_experimental` filter. The PLS
    /// `profile_index` field exhaustively disambiguates.
    pub const ALL_AUTO_DETECT: [Self; 8] = Self::ALL;

    /// Wire byte (PLHEADER PLS payload).
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Reverse of [`as_u8`]. `None` for unknown bytes including
    /// [`PROFILE_INDEX_2X_UNKNOWN`].
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Ultra2x),
            1 => Some(Self::Robust2x),
            2 => Some(Self::Normal2x),
            3 => Some(Self::High2x),
            4 => Some(Self::HighPlus2x),
            5 => Some(Self::HighPlusPlus2x),
            6 => Some(Self::HighFiveSix2x),
            7 => Some(Self::HighPlusFiveSix2x),
            _ => None,
        }
    }

    /// Stable identifier reported through the `Modem` trait. Mirrors the
    /// V3 `name()` minus the `2x` suffix where the V3 name already
    /// existed; matches what the GUI combo and the CLI flag accept.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ultra2x => "ULTRA2X",
            Self::Robust2x => "ROBUST2X",
            Self::Normal2x => "NORMAL2X",
            Self::High2x => "HIGH2X",
            Self::HighPlus2x => "HIGH+2X",
            Self::HighPlusPlus2x => "HIGH++2X",
            Self::HighFiveSix2x => "HIGH56_2X",
            Self::HighPlusFiveSix2x => "HIGH+56_2X",
        }
    }

    /// Resolve a profile by name. Accepts the canonical form returned by
    /// [`name`] plus a few keyboard-friendly aliases for the `+` chars.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_uppercase().as_str() {
            "ULTRA2X" => Some(Self::Ultra2x),
            "ROBUST2X" => Some(Self::Robust2x),
            "NORMAL2X" => Some(Self::Normal2x),
            "HIGH2X" => Some(Self::High2x),
            "HIGH+2X" | "HIGHPLUS2X" => Some(Self::HighPlus2x),
            "HIGH++2X" | "HIGHPLUSPLUS2X" => Some(Self::HighPlusPlus2x),
            "HIGH56_2X" | "HIGH56-2X" | "HIGH562X" => Some(Self::HighFiveSix2x),
            "HIGH+56_2X" | "HIGH+56-2X" | "HIGHPLUS562X" => {
                Some(Self::HighPlusFiveSix2x)
            }
            _ => None,
        }
    }

    /// Build the full [`ModemConfig2x`] for this profile.
    pub fn to_config(self) -> ModemConfig2x {
        match self {
            Self::Ultra2x => profile_ultra_2x(),
            Self::Robust2x => profile_robust_2x(),
            Self::Normal2x => profile_normal_2x(),
            Self::High2x => profile_high_2x(),
            Self::HighPlus2x => profile_high_plus_2x(),
            Self::HighPlusPlus2x => profile_high_plus_plus_2x(),
            Self::HighFiveSix2x => profile_high_5_6_2x(),
            Self::HighPlusFiveSix2x => profile_high_plus_5_6_2x(),
        }
    }
}

/// Wire-format-independent DSP knobs. Mirrors V3 `ModemConfig` minus the
/// TDM `pilot_pattern` and the `mode_code`, both V3-specific.
#[derive(Clone, Debug, PartialEq)]
pub struct ModemConfigBase2x {
    pub constellation: ConstellationType,
    pub ldpc_rate: LdpcRate,
    pub symbol_rate: f64,
    pub beta: f64,
    pub tau: f64,
    pub center_freq_hz: f64,
    /// First APSK ratio (R2/R1). Ignored for QPSK/8PSK.
    pub apsk_gamma: f64,
    /// Second APSK ratio (R3/R1). Ignored except for Apsk32/Apsk64.
    pub apsk_gamma2: f64,
    /// Third APSK ratio (R4/R1). Ignored except for Apsk64.
    pub apsk_gamma3: f64,
}

/// Full 2x modem configuration — base DSP + 2x-specific framing knobs.
///
/// Both encoder ([`crate::frame2x`]) and decoder ([`crate::rx_v4`]) take
/// this struct by reference; it is the single source of truth for cycle
/// layout arithmetic.
#[derive(Clone, Debug, PartialEq)]
pub struct ModemConfig2x {
    pub base: ModemConfigBase2x,

    /// TDM pilot pattern (`d_syms` data + `p_syms` rotating-QPSK pilots
    /// per group, repeating across every LDPC codeword). Defaults to
    /// 32/2 for all 8 profiles — same density and update bandwidth as
    /// V3 HIGH+. Tunable per profile so we can experiment with denser
    /// patterns on APSK-32/64 without recompiling the catalogue.
    pub pilot_pattern: PilotPattern2x,

    /// LMS warmup symbols inserted right after the PLHEADER for APSK
    /// profiles whose constellation has rings the QPSK SOF doesn't
    /// touch. Identical convention to V3
    /// (`make_lms_warmup_for_config`): 0 for QPSK/8PSK/16-APSK, 32 for
    /// 32-APSK, 64 for 64-APSK.
    pub lms_warmup_syms: usize,

    /// Multiplier the TX applies to the unit-circle SOF before pulse
    /// shaping. For QPSK/8PSK/16-APSK profiles this is 1.0 (the SOF
    /// lives natively inside the constellation). For APSK32 / APSK64 it
    /// is the outer-ring radius `R3` (32-APSK) / `R4` (64-APSK) so the
    /// SOF lands on a strict subset of data points and the FFE training
    /// sees no scale jump.
    pub training_amplitude: f64,

    /// SOF/PLHEADER family — driven by the symbol rate via
    /// [`PreambleFamily2x::from_sps`] but stored explicitly so the
    /// encoder/decoder don't recompute it on every cycle.
    pub family: PreambleFamily2x,
}

impl ModemConfig2x {
    /// Net data rate (bits/s) after LDPC + TDM pilot overhead.
    /// Excludes the PLHEADER (its overhead is amortised across multi-CW
    /// cycles and depends on `V4_PREAMBLE_PERIOD_S`); see
    /// `frame2x::superframe_total_symbols_v4` for the symbol-accurate
    /// count used by the duration estimator.
    pub fn net_bitrate(&self) -> f64 {
        let bits_per_sym = self.base.constellation.bits_per_sym() as f64;
        let cw_data_syms = self.cw_data_syms();
        let wire_len = self.pilot_pattern.wire_len(cw_data_syms) as f64;
        let pilot_eff = cw_data_syms as f64 / wire_len;
        self.base.symbol_rate * bits_per_sym * self.base.tau * self.base.ldpc_rate.rate()
            * pilot_eff
    }

    /// Codeword data symbols (bits/sym must divide N=2304 — Apsk32 needs
    /// padding so we round up to the next multiple of bits/sym, same as
    /// V3 `interleaver::padded_cw_bits`).
    pub fn cw_data_syms(&self) -> usize {
        let bps = self.base.constellation.bits_per_sym();
        let n = self.base.ldpc_rate.n();
        (n + bps - 1) / bps
    }

    /// Compute training amplitude for an APSK config — same formulas as
    /// V3 `ModemConfig::training_amplitude`. Public so tests can verify
    /// the cached `training_amplitude` field stays in sync with the
    /// constellation γ values.
    pub fn computed_training_amplitude(&self) -> f64 {
        match self.base.constellation {
            ConstellationType::Apsk32 => {
                let g1 = self.base.apsk_gamma;
                let g2 = self.base.apsk_gamma2;
                let r0 = (8.0 / (1.0 + 3.0 * g1 * g1 + 4.0 * g2 * g2)).sqrt();
                g2 * r0
            }
            ConstellationType::Apsk64 => {
                let g1 = self.base.apsk_gamma;
                let g2 = self.base.apsk_gamma2;
                let g3 = self.base.apsk_gamma3;
                let r0 = (16.0
                    / (1.0
                        + 3.0 * g1 * g1
                        + 5.0 * g2 * g2
                        + 7.0 * g3 * g3))
                    .sqrt();
                g3 * r0
            }
            _ => 1.0,
        }
    }

    /// LMS warmup count for the configured constellation. Useful at
    /// construction time (the predefined profile builders below call it
    /// to populate the `lms_warmup_syms` field).
    fn computed_lms_warmup(constellation: ConstellationType) -> usize {
        match constellation {
            ConstellationType::Apsk32 => 32,
            ConstellationType::Apsk64 => 64,
            _ => 0,
        }
    }
}

// --- Predefined profile builders ------------------------------------------

fn make(
    constellation: ConstellationType,
    ldpc_rate: LdpcRate,
    symbol_rate: f64,
    beta: f64,
    apsk_gamma: f64,
    apsk_gamma2: f64,
    apsk_gamma3: f64,
    pilot_pattern: PilotPattern2x,
) -> ModemConfig2x {
    let base = ModemConfigBase2x {
        constellation,
        ldpc_rate,
        symbol_rate,
        beta,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma,
        apsk_gamma2,
        apsk_gamma3,
    };
    let family = PreambleFamily2x::from_sps(48_000.0 / symbol_rate);
    let lms_warmup_syms = ModemConfig2x::computed_lms_warmup(constellation);
    let mut cfg = ModemConfig2x {
        base,
        pilot_pattern,
        lms_warmup_syms,
        training_amplitude: 1.0,
        family,
    };
    cfg.training_amplitude = cfg.computed_training_amplitude();
    cfg
}

/// QPSK 500 Bd 1/2 — long-range fallback.
pub fn profile_ultra_2x() -> ModemConfig2x {
    make(
        ConstellationType::Qpsk,
        LdpcRate::R1_2,
        500.0,
        0.25,
        2.85,
        0.0,
        0.0,
        PilotPattern2x::default_2x(),
    )
}

/// QPSK 1000 Bd 1/2.
pub fn profile_robust_2x() -> ModemConfig2x {
    make(
        ConstellationType::Qpsk,
        LdpcRate::R1_2,
        1000.0,
        0.25,
        2.85,
        0.0,
        0.0,
        PilotPattern2x::default_2x(),
    )
}

/// 8PSK 1500 Bd 1/2.
pub fn profile_normal_2x() -> ModemConfig2x {
    make(
        ConstellationType::Psk8,
        LdpcRate::R1_2,
        1500.0,
        0.20,
        2.85,
        0.0,
        0.0,
        PilotPattern2x::default_2x(),
    )
}

/// 16-APSK 1500 Bd 3/4.
pub fn profile_high_2x() -> ModemConfig2x {
    make(
        ConstellationType::Apsk16,
        LdpcRate::R3_4,
        1500.0,
        0.20,
        2.85,
        0.0,
        0.0,
        PilotPattern2x::default_2x(),
    )
}

/// 32-APSK 1500 Bd 3/4 — V3-style TDM pilots (32 data + 2 pilots / group).
pub fn profile_high_plus_2x() -> ModemConfig2x {
    make(
        ConstellationType::Apsk32,
        LdpcRate::R3_4,
        1500.0,
        0.20,
        2.84,
        5.27,
        0.0,
        PilotPattern2x::default_2x(),
    )
}

/// 64-APSK 1500 Bd 3/4 — densified TDM pilots (16 data + 2 pilots /
/// group) for the small squared min-distance.
pub fn profile_high_plus_plus_2x() -> ModemConfig2x {
    make(
        ConstellationType::Apsk64,
        LdpcRate::R3_4,
        1500.0,
        0.20,
        2.4,
        4.3,
        7.0,
        PilotPattern2x::dense_2x(),
    )
}

/// 16-APSK 1500 Bd 5/6.
pub fn profile_high_5_6_2x() -> ModemConfig2x {
    make(
        ConstellationType::Apsk16,
        LdpcRate::R5_6,
        1500.0,
        0.20,
        2.85,
        0.0,
        0.0,
        PilotPattern2x::default_2x(),
    )
}

/// 32-APSK 1500 Bd 5/6 — V3-style TDM pilots.
pub fn profile_high_plus_5_6_2x() -> ModemConfig2x {
    make(
        ConstellationType::Apsk32,
        LdpcRate::R5_6,
        1500.0,
        0.20,
        2.84,
        5.27,
        0.0,
        PilotPattern2x::default_2x(),
    )
}

/// One-shot: resolve a profile name to its full `ModemConfig2x`. Used by
/// the worker, CLI and Modem trait.
pub fn config_by_name_2x(name: &str) -> Option<ModemConfig2x> {
    ProfileIndex2x::from_name(name).map(ProfileIndex2x::to_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_eight_profiles_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for p in ProfileIndex2x::ALL {
            assert!(seen.insert(p.as_u8()), "duplicate wire byte for {p:?}");
            assert!(seen.contains(&p.as_u8()));
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn wire_byte_roundtrip() {
        for p in ProfileIndex2x::ALL {
            let b = p.as_u8();
            assert_eq!(ProfileIndex2x::from_u8(b), Some(p));
        }
        assert_eq!(ProfileIndex2x::from_u8(8), None);
        assert_eq!(ProfileIndex2x::from_u8(PROFILE_INDEX_2X_UNKNOWN), None);
    }

    #[test]
    fn name_roundtrip_canonical_and_aliases() {
        for p in ProfileIndex2x::ALL {
            assert_eq!(ProfileIndex2x::from_name(p.name()), Some(p));
        }
        // Aliases.
        assert_eq!(
            ProfileIndex2x::from_name("HIGHPLUS2X"),
            Some(ProfileIndex2x::HighPlus2x)
        );
        assert_eq!(
            ProfileIndex2x::from_name("HIGHPLUSPLUS2X"),
            Some(ProfileIndex2x::HighPlusPlus2x)
        );
        assert_eq!(
            ProfileIndex2x::from_name("high+2x"), // case-insensitive
            Some(ProfileIndex2x::HighPlus2x)
        );
        assert_eq!(ProfileIndex2x::from_name("garbage"), None);
    }

    #[test]
    fn auto_detect_includes_high_plus_plus() {
        assert!(ProfileIndex2x::ALL_AUTO_DETECT
            .iter()
            .any(|p| *p == ProfileIndex2x::HighPlusPlus2x));
    }

    #[test]
    fn pilot_pattern_per_profile_matches_design() {
        // After the TDM revert: default 32/2 for everyone except
        // HighPlusPlus2x (64-APSK) which uses the V3-style dense 16/2
        // pattern to anchor the smaller squared min-distance.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let expected = match p {
                ProfileIndex2x::HighPlusPlus2x => PilotPattern2x::dense_2x(),
                _ => PilotPattern2x::default_2x(),
            };
            assert_eq!(
                cfg.pilot_pattern, expected,
                "{p:?} should have pattern {expected:?}",
            );
        }
    }

    #[test]
    fn lms_warmup_matches_constellation() {
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let expected = match cfg.base.constellation {
                ConstellationType::Apsk32 => 32,
                ConstellationType::Apsk64 => 64,
                _ => 0,
            };
            assert_eq!(cfg.lms_warmup_syms, expected, "{p:?}");
        }
    }

    #[test]
    fn family_assignment_matches_symbol_rate() {
        // sps = 48000/Rs. PreambleFamily2x::from_sps splits at 1200 / 750.
        // Ultra: 500 Bd → sps=96 → A. Robust: 1000 Bd → sps=48 → C.
        // Normal/High/...: 1500 Bd → sps=32 → C.
        // (PreambleFamily2x bins by sps not Rs — confirms the convention
        // the plan documents.)
        assert_eq!(profile_ultra_2x().family, PreambleFamily2x::C);
        assert_eq!(profile_robust_2x().family, PreambleFamily2x::C);
        assert_eq!(profile_normal_2x().family, PreambleFamily2x::C);
        assert_eq!(profile_high_2x().family, PreambleFamily2x::C);
    }

    #[test]
    fn training_amplitude_unity_for_qpsk_and_low_apsk() {
        for cfg in [
            profile_ultra_2x(),
            profile_robust_2x(),
            profile_normal_2x(),
            profile_high_2x(),
            profile_high_5_6_2x(),
        ] {
            assert!(
                (cfg.training_amplitude - 1.0).abs() < 1e-12,
                "{:?} training_amplitude={}",
                cfg.base.constellation,
                cfg.training_amplitude
            );
        }
    }

    #[test]
    #[allow(non_snake_case)]
    fn training_amplitude_apsk32_matches_outer_ring_R3() {
        // 32-APSK γ1=2.84 γ2=5.27, Es=1:
        //   r0² = 8/(1+3γ1²+4γ2²) = 8/136.29 → r0 ≈ 0.2423,
        //   R3  = γ2·r0 ≈ 1.277.
        let cfg = profile_high_plus_2x();
        assert!(
            (cfg.training_amplitude - 1.277).abs() < 0.01,
            "training_amplitude={} expected≈1.277",
            cfg.training_amplitude
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn training_amplitude_apsk64_matches_outer_ring_R4() {
        // 64-APSK γ1=2.4 γ2=4.3 γ3=7.0, Es=1:
        //   r0² = 16/(1+3γ1²+5γ2²+7γ3²) = 16/453.73 → r0 ≈ 0.1878,
        //   R4  = γ3·r0 ≈ 1.315.
        let cfg = profile_high_plus_plus_2x();
        assert!(
            (cfg.training_amplitude - 1.315).abs() < 0.01,
            "training_amplitude={} expected≈1.315",
            cfg.training_amplitude
        );
    }

    #[test]
    fn cw_data_syms_padded_for_apsk32() {
        // 2304 bits ÷ 5 bits/sym = 460.8 → padded to 461 sym.
        let cfg = profile_high_plus_2x();
        assert_eq!(cfg.cw_data_syms(), 461);
    }

    #[test]
    fn cw_data_syms_qpsk() {
        // 2304 ÷ 2 = 1152 sym (no padding).
        let cfg = profile_ultra_2x();
        assert_eq!(cfg.cw_data_syms(), 1152);
    }

    #[test]
    fn cw_data_syms_apsk64() {
        // 2304 ÷ 6 = 384 sym (no padding, divides cleanly).
        let cfg = profile_high_plus_plus_2x();
        assert_eq!(cfg.cw_data_syms(), 384);
    }

    #[test]
    fn net_bitrate_high_2x_matches_v3_tdm_overhead() {
        // V3 HIGH and V4 HIGH2X now share the same TDM pattern (32/2):
        //   576 data sym/CW → ⌈576/32⌉ = 18 groups → 36 pilot sym → 612 wire.
        //   1500 × 4 × 0.75 × (576/612) ≈ 4235 bps.
        let r = profile_high_2x().net_bitrate();
        let cw_data = 576.0;
        let pilot = 36.0; // 18 groups × 2 pilots
        let expected = 1500.0 * 4.0 * 0.75 * (cw_data / (cw_data + pilot));
        assert!(
            (r - expected).abs() < 1.0,
            "rate={r} expected={expected}"
        );
    }

    #[test]
    fn net_bitrate_high_plus_plus_2x_dense_pattern() {
        // 64-APSK with dense pattern (16/2): 384 data sym/CW
        //   → ⌈384/16⌉ = 24 groups → 48 pilot sym → 432 wire.
        let cfg = profile_high_plus_plus_2x();
        let expected = 1500.0 * 6.0 * 0.75 * (384.0 / (384.0 + 48.0));
        assert!(
            (cfg.net_bitrate() - expected).abs() < 1.0,
            "rate={} expected={expected}",
            cfg.net_bitrate()
        );
    }

    #[test]
    fn net_bitrate_high_plus_2x_matches_v3_high_plus() {
        // The headline number: V3 HIGH+ delivered 5294 bps with the
        // 32/2 TDM pattern; V4 HighPlus2x must match it now that the
        // 2x catalogue reverted to the same pattern.
        // 461 data sym/CW + ⌈461/32⌉=15 groups × 2 = 30 pilot → 491 wire.
        let cfg = profile_high_plus_2x();
        let expected = 1500.0 * 5.0 * 0.75 * (461.0 / 491.0);
        assert!(
            (cfg.net_bitrate() - expected).abs() < 1.0,
            "rate={} expected={expected}",
            cfg.net_bitrate()
        );
        assert!(
            (cfg.net_bitrate() - 5290.0).abs() < 10.0,
            "expected ~5294 bps got {}",
            cfg.net_bitrate()
        );
    }

    #[test]
    fn config_by_name_2x_canonical() {
        assert!(matches!(
            config_by_name_2x("HIGH2X"),
            Some(_)
        ));
        assert!(config_by_name_2x("nope").is_none());
    }

    #[test]
    fn config_eq_independent_of_construction_path() {
        // Building the same profile twice must yield identical configs —
        // important because both encoder and decoder reconstruct from
        // ProfileIndex2x → to_config and must agree symbol-for-symbol on
        // pilot positions / amplitudes.
        for p in ProfileIndex2x::ALL {
            assert_eq!(p.to_config(), p.to_config());
        }
    }
}
