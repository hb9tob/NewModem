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
}

impl ModemConfig {
    /// Net data rate in bits/s (after LDPC + pilot overhead).
    pub fn net_bitrate(&self) -> f64 {
        let gross = self.symbol_rate * self.constellation.bits_per_sym() as f64 * self.tau;
        let pilot_eff = D_SYMS_F / (D_SYMS_F + P_SYMS_F);
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
        })
    }
}

const D_SYMS_F: f64 = crate::types::D_SYMS as f64;
const P_SYMS_F: f64 = crate::types::P_SYMS as f64;

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
}
