//! Modem configuration and predefined profiles.
//!
//! A mode is defined by: constellation type + LDPC rate + symbol rate + RRC params + center freq.
//! Predefined profiles (MEGA, HIGH, NORMAL, ROBUST, ULTRA) are convenience presets.

use crate::types::DATA_CENTER_HZ;

/// Constellation type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstellationType {
    Qpsk,
    Psk8,
    Apsk16,
}

impl ConstellationType {
    pub fn bits_per_sym(self) -> usize {
        match self {
            Self::Qpsk => 2,
            Self::Psk8 => 3,
            Self::Apsk16 => 4,
        }
    }

    /// Encoding byte for the header mode_code field.
    pub fn code(self) -> u8 {
        match self {
            Self::Qpsk => 0,
            Self::Psk8 => 1,
            Self::Apsk16 => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Qpsk),
            1 => Some(Self::Psk8),
            2 => Some(Self::Apsk16),
            _ => None,
        }
    }
}

/// LDPC code rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LdpcRate {
    R1_2,
    R2_3,
    R3_4,
}

impl LdpcRate {
    /// Numeric rate value.
    pub fn rate(self) -> f64 {
        match self {
            Self::R1_2 => 0.5,
            Self::R2_3 => 2.0 / 3.0,
            Self::R3_4 => 0.75,
        }
    }

    /// Info bits per codeword (N=2304).
    pub fn k(self) -> usize {
        match self {
            Self::R1_2 => 1152,
            Self::R2_3 => 1536,
            Self::R3_4 => 1728,
        }
    }

    /// Codeword length.
    pub fn n(self) -> usize {
        2304
    }

    /// Encoding byte for header.
    pub fn code(self) -> u8 {
        match self {
            Self::R1_2 => 0,
            Self::R2_3 => 1,
            Self::R3_4 => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::R1_2),
            1 => Some(Self::R2_3),
            2 => Some(Self::R3_4),
            _ => None,
        }
    }
}

/// TDM pilot pattern: one group = `d_syms` data + `p_syms` QPSK pilots.
///
/// Historically this was a global constant (`types::D_SYMS` = 32, `P_SYMS` = 2).
/// It is now a per-profile knob so low-rate profiles can densify pilots and
/// get a finer phase tracking grid without paying the overhead on higher-rate
/// profiles where the standard spacing is already well inside Nyquist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PilotPattern {
    pub d_syms: usize,
    pub p_syms: usize,
}

impl PilotPattern {
    /// 32 data + 2 pilots per group — v3 default, used by HIGH/NORMAL/MEGA/ROBUST.
    pub const fn default_v3() -> Self {
        Self { d_syms: 32, p_syms: 2 }
    }

    /// 16 data + 2 pilots per group — densified, specific to ULTRA (Rs=500 Bd)
    /// to double the pilot sampling rate on a profile where the 68 ms gap of
    /// the default pattern aliases sub-Nyquist drift components.
    pub const fn dense_ultra() -> Self {
        Self { d_syms: 16, p_syms: 2 }
    }

    /// Total symbols per group (data + pilots).
    pub const fn group_sz(&self) -> usize {
        self.d_syms + self.p_syms
    }
}

/// Full modem configuration.
#[derive(Clone, Debug)]
pub struct ModemConfig {
    pub constellation: ConstellationType,
    pub ldpc_rate: LdpcRate,
    pub symbol_rate: f64,
    pub beta: f64,
    pub tau: f64,
    pub center_freq_hz: f64,
    /// APSK gamma (only used for Apsk16, default 2.85)
    pub apsk_gamma: f64,
    /// TDM pilot pattern. Profile-specific so ULTRA can densify.
    pub pilot_pattern: PilotPattern,
}

/// Canonical profile identifier.
///
/// Transported as a single byte in the protocol header's `profile_index` field
/// so the RX can unambiguously reconstruct the full `ModemConfig` (including
/// `tau` and `beta`, which are NOT captured by `mode_code`). A byte here also
/// resolves the HIGH vs MEGA ambiguity (same constellation + rate + symbol_rate
/// → same mode_code; only `tau` differs).
///
/// Value 0xFF reserved for "unknown / legacy / not-set".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProfileIndex {
    Ultra = 0,
    Robust = 1,
    Normal = 2,
    High = 3,
    Mega = 4,
}

impl ProfileIndex {
    pub const UNKNOWN: u8 = 0xFF;

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Ultra),
            1 => Some(Self::Robust),
            2 => Some(Self::Normal),
            3 => Some(Self::High),
            4 => Some(Self::Mega),
            _ => None,
        }
    }

    /// Build the full `ModemConfig` for this profile.
    pub fn to_config(self) -> ModemConfig {
        match self {
            Self::Ultra => profile_ultra(),
            Self::Robust => profile_robust(),
            Self::Normal => profile_normal(),
            Self::High => profile_high(),
            Self::Mega => profile_mega(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ultra => "ULTRA",
            Self::Robust => "ROBUST",
            Self::Normal => "NORMAL",
            Self::High => "HIGH",
            Self::Mega => "MEGA",
        }
    }

    /// All profile indices in canonical order.
    pub const ALL: [Self; 5] = [
        Self::Ultra,
        Self::Robust,
        Self::Normal,
        Self::High,
        Self::Mega,
    ];

    /// Preamble family used by this profile on the wire. The split is
    /// driven by `(sps, β)` — profiles that share both end up in the same
    /// family and emit the same preamble sequence ; the header's
    /// `profile_index` byte still disambiguates them downstream.
    pub fn preamble_family(self) -> crate::preamble::PreambleFamily {
        use crate::preamble::PreambleFamily;
        match self {
            Self::Normal | Self::High | Self::Mega => PreambleFamily::A,
            Self::Robust => PreambleFamily::B,
            Self::Ultra => PreambleFamily::C,
        }
    }

    /// Default profile to assume after the FFT gate identifies a family
    /// but before the protocol header refines the exact index. Within
    /// family A the canonical anchor is NORMAL (the ProfileIndex byte
    /// in the Golay-decoded header switches to HIGH or MEGA on demand) ;
    /// families B and C have a single profile each.
    pub fn anchor_for_family(family: crate::preamble::PreambleFamily) -> Self {
        use crate::preamble::PreambleFamily;
        match family {
            PreambleFamily::A => Self::Normal,
            PreambleFamily::B => Self::Robust,
            PreambleFamily::C => Self::Ultra,
        }
    }
}

impl ModemConfig {
    /// Preamble family this config emits / expects. Routed through the
    /// matching `(symbol_rate, β)` pair so configs built outside of the
    /// canonical profiles still pick a sensible family. Falls back to
    /// `PreambleFamily::A` for unknown combinations (matches the legacy
    /// behaviour before the family split).
    pub fn preamble_family(&self) -> crate::preamble::PreambleFamily {
        use crate::preamble::PreambleFamily;
        let rs = self.symbol_rate.round() as u32;
        match rs {
            r if r >= 1200 => PreambleFamily::A, // 1500 Bd group
            r if r >= 750 => PreambleFamily::B,  // 1000 Bd group
            _ => PreambleFamily::C,              // 500 Bd group
        }
    }
}

impl ModemConfig {
    /// Net data rate in bits/s (after LDPC + pilot overhead).
    pub fn net_bitrate(&self) -> f64 {
        let gross = self.symbol_rate * self.constellation.bits_per_sym() as f64 * self.tau;
        let d = self.pilot_pattern.d_syms as f64;
        let p = self.pilot_pattern.p_syms as f64;
        let pilot_eff = d / (d + p);
        gross * self.ldpc_rate.rate() * pilot_eff
    }

    /// Encode the mode into a single byte for the header.
    /// Format: [constellation:2][ldpc_rate:2][rs_index:4]
    pub fn mode_code(&self) -> u8 {
        let c = self.constellation.code();
        let l = self.ldpc_rate.code();
        let rs_idx = symbol_rate_index(self.symbol_rate);
        (c << 6) | (l << 4) | (rs_idx & 0x0F)
    }

    /// Decode a mode_code byte.
    ///
    /// The mode_code does not carry the pilot pattern — it's reconstructed via
    /// `ProfileIndex::to_config()` in the real RX path. This helper assumes
    /// the v3 default pattern; it's used for header-level round-trip tests only.
    pub fn from_mode_code(code: u8, beta: f64, tau: f64, center_freq_hz: f64) -> Option<Self> {
        let c = ConstellationType::from_code((code >> 6) & 0x03)?;
        let l = LdpcRate::from_code((code >> 4) & 0x03)?;
        let rs = symbol_rate_from_index(code & 0x0F)?;
        Some(ModemConfig {
            constellation: c,
            ldpc_rate: l,
            symbol_rate: rs,
            beta,
            tau,
            center_freq_hz,
            apsk_gamma: 2.85,
            pilot_pattern: PilotPattern::default_v3(),
        })
    }
}

/// Known symbol rates (index -> rate).
const SYMBOL_RATES: [(u8, f64); 7] = [
    (0, 500.0),
    (1, 600.0),
    (2, 750.0),
    (3, 1000.0),
    (4, 1200.0),
    (5, 1500.0),
    (6, 2000.0),
];

fn symbol_rate_index(rs: f64) -> u8 {
    SYMBOL_RATES
        .iter()
        .find(|(_, r)| (*r - rs).abs() < 1.0)
        .map(|(idx, _)| *idx)
        .unwrap_or(5) // default to 1500
}

fn symbol_rate_from_index(idx: u8) -> Option<f64> {
    SYMBOL_RATES
        .iter()
        .find(|(i, _)| *i == idx)
        .map(|(_, r)| *r)
}

// --- Predefined profiles ---

pub fn profile_mega() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Apsk16,
        ldpc_rate: LdpcRate::R3_4,
        symbol_rate: 1500.0,
        beta: 0.20,
        tau: 30.0 / 32.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.85,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

pub fn profile_high() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Apsk16,
        ldpc_rate: LdpcRate::R3_4,
        symbol_rate: 1500.0,
        beta: 0.20,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.85,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

pub fn profile_normal() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Psk8,
        ldpc_rate: LdpcRate::R1_2,
        symbol_rate: 1500.0,
        beta: 0.20,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.85,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

pub fn profile_robust() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Qpsk,
        ldpc_rate: LdpcRate::R1_2,
        symbol_rate: 1000.0,
        beta: 0.25,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.85,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

pub fn profile_ultra() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Qpsk,
        ldpc_rate: LdpcRate::R1_2,
        symbol_rate: 500.0,
        beta: 0.25,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.85,
        pilot_pattern: PilotPattern::dense_ultra(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_code_roundtrip() {
        for profile_fn in [profile_mega, profile_high, profile_normal, profile_robust, profile_ultra] {
            let cfg = profile_fn();
            let code = cfg.mode_code();
            let decoded = ModemConfig::from_mode_code(code, cfg.beta, cfg.tau, cfg.center_freq_hz).unwrap();
            assert_eq!(decoded.constellation, cfg.constellation);
            assert_eq!(decoded.ldpc_rate, cfg.ldpc_rate);
            assert!((decoded.symbol_rate - cfg.symbol_rate).abs() < 1.0);
        }
    }

    #[test]
    fn net_bitrate_normal() {
        let cfg = profile_normal();
        let rate = cfg.net_bitrate();
        // 8PSK 1500 Bd rate 1/2: 1500 * 3 * 0.5 * (32/34) ≈ 2117 bps
        assert!((rate - 2117.6).abs() < 5.0, "rate = {rate}");
    }

    #[test]
    fn ultra_uses_dense_pilot_pattern() {
        // ULTRA is the only profile that densifies its pilot layout. Everything
        // else keeps the v3 default (32/2). This test guards that invariant —
        // a regression would silently halve ULTRA's drift tracking bandwidth.
        assert_eq!(profile_ultra().pilot_pattern, PilotPattern::dense_ultra());
        for cfg in [profile_mega(), profile_high(), profile_normal(), profile_robust()] {
            assert_eq!(
                cfg.pilot_pattern,
                PilotPattern::default_v3(),
                "non-ULTRA profile should use default_v3 pattern",
            );
        }
    }

    #[test]
    fn net_bitrate_ultra_dense_pattern() {
        let cfg = profile_ultra();
        let rate = cfg.net_bitrate();
        // QPSK 500 Bd rate 1/2 dense pilots (16/2): 500 * 2 * 0.5 * (16/18) ≈ 444 bps
        assert!((rate - 444.4).abs() < 1.0, "rate = {rate}");
    }
}
