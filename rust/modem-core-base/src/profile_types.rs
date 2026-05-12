//! Shared profile enums used by both V3 (`modem-core`) and 2x
//! (`modem-core2x`).
//!
//! Only the pieces of the profile vocabulary that are wire-format and
//! frame-format independent live here: the `ConstellationType` enum (which
//! constellation to map onto) and the `LdpcRate` enum (which WiMAX
//! 802.16e rate to encode with). The frame-specific decorations
//! (pilot pattern, training amplitude, LMS warmup count, etc.) stay in
//! the V3 `modem_core::profile` or V4 `modem_core2x::profile2x` modules.

/// Constellation type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstellationType {
    Qpsk,
    Psk8,
    Apsk16,
    /// 32-APSK DVB-S2 (4+12+16). EXPERIMENTAL -- not covered by the
    /// gate's auto-detection, usable only in RX forced mode.
    Apsk32,
    /// 64-APSK DVB-S2X (4+12+20+28). EXPERIMENTAL -- not covered by the
    /// gate's auto-detection, usable only in RX forced mode.
    /// code() returns 4 (5th constellation) which overflows the 2-bit
    /// mode_code field -> truncated to 0 (visual alias of Qpsk in
    /// mode_code); `profile_index` remains the authority for
    /// reconstructing the ModemConfig.
    Apsk64,
}

impl ConstellationType {
    pub fn bits_per_sym(self) -> usize {
        match self {
            Self::Qpsk => 2,
            Self::Psk8 => 3,
            Self::Apsk16 => 4,
            Self::Apsk32 => 5,
            Self::Apsk64 => 6,
        }
    }

    /// Encoding byte for the header mode_code field.
    ///
    /// The field is only 2 bits in `mode_code()`; values >3 (Apsk64=4)
    /// are truncated by `mode_code()` and disambiguation goes through
    /// `profile_index` (cf. ProfileIndex::HighPlusPlus).
    pub fn code(self) -> u8 {
        match self {
            Self::Qpsk => 0,
            Self::Psk8 => 1,
            Self::Apsk16 => 2,
            Self::Apsk32 => 3,
            Self::Apsk64 => 4,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Qpsk),
            1 => Some(Self::Psk8),
            2 => Some(Self::Apsk16),
            3 => Some(Self::Apsk32),
            // Apsk64=4 CANNOT be encoded in 2 bits; disambiguation goes
            // through profile_index, not mode_code.
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
    /// 5/6 = 0.8333. IEEE 802.16e matrix (RPTU alist), k=1920, n=2304.
    /// Used by the HIGH56 and HIGH+56 profiles (experimental) to scrape
    /// +11% of throughput vs their 3/4 counterparts -- at the cost of
    /// ~0.7 dB less LDPC margin.
    R5_6,
}

impl LdpcRate {
    /// Numeric rate value.
    pub fn rate(self) -> f64 {
        match self {
            Self::R1_2 => 0.5,
            Self::R2_3 => 2.0 / 3.0,
            Self::R3_4 => 0.75,
            Self::R5_6 => 5.0 / 6.0,
        }
    }

    /// Info bits per codeword (N=2304).
    pub fn k(self) -> usize {
        match self {
            Self::R1_2 => 1152,
            Self::R2_3 => 1536,
            Self::R3_4 => 1728,
            Self::R5_6 => 1920,
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
            Self::R5_6 => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::R1_2),
            1 => Some(Self::R2_3),
            2 => Some(Self::R3_4),
            3 => Some(Self::R5_6),
            _ => None,
        }
    }
}
