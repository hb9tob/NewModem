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
    /// APSK gamma (Apsk16: R2/R1, default 2.85; Apsk32/Apsk64: R2/R1,
    /// e.g. 2.84 for 32-APSK rate 3/4, 2.4 for 64-APSK rate 11/15).
    /// For Qpsk/Psk8: ignored.
    pub apsk_gamma: f64,
    /// 2nd APSK gamma -- used by Apsk32 (R3/R1, e.g. 5.27 for rate 3/4)
    /// and Apsk64 (R3/R1, e.g. 4.3 for rate 11/15). For the other
    /// constellations: ignored (leave at 0.0).
    pub apsk_gamma2: f64,
    /// 3rd APSK gamma -- used only by Apsk64 (R4/R1, e.g. 7.0 for
    /// rate 11/15). For the other constellations: ignored (0.0).
    pub apsk_gamma3: f64,
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
    /// 32-APSK 1500 Bd beta=0.20 LDPC 3/4. **Standard** profile since OTA
    /// HB9MM 2026-04 (validated in practice, throughput/margin
    /// equivalent to HIGH).
    HighPlus = 5,
    /// EXPERIMENTAL -- 16-APSK 1714 Bd beta=0.15 LDPC 3/4. Outside the
    /// auto-detection, usable only when the RX is in forced mode.
    Fast = 6,
    /// EXPERIMENTAL -- 64-APSK DVB-S2X 1500 Bd beta=0.20 LDPC 3/4.
    /// Outside the auto-detection, usable only when the RX is in
    /// forced mode.
    HighPlusPlus = 7,
    /// EXPERIMENTAL -- 16-APSK 1500 Bd beta=0.20 LDPC **5/6**. +11% of
    /// throughput vs HIGH (4706 vs 4235 bps) at the cost of ~0.7 dB of
    /// margin.
    HighFiveSix = 8,
    /// EXPERIMENTAL -- 32-APSK 1500 Bd beta=0.20 LDPC **5/6**. +11% of
    /// throughput vs HIGH+ (5882 vs 5294 bps) at the cost of ~0.7 dB of
    /// margin.
    HighPlusFiveSix = 9,
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
            5 => Some(Self::HighPlus),
            6 => Some(Self::Fast),
            7 => Some(Self::HighPlusPlus),
            8 => Some(Self::HighFiveSix),
            9 => Some(Self::HighPlusFiveSix),
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
            Self::HighPlus => profile_high_plus(),
            Self::Fast => profile_fast(),
            Self::HighPlusPlus => profile_high_plus_plus(),
            Self::HighFiveSix => profile_high_5_6(),
            Self::HighPlusFiveSix => profile_high_plus_5_6(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ultra => "ULTRA",
            Self::Robust => "ROBUST",
            Self::Normal => "NORMAL",
            Self::High => "HIGH",
            Self::Mega => "MEGA",
            Self::HighPlus => "HIGH+",
            Self::Fast => "FAST",
            Self::HighPlusPlus => "HIGH++",
            Self::HighFiveSix => "HIGH56",
            Self::HighPlusFiveSix => "HIGH+56",
        }
    }

    /// Resolve a profile by name. Accepts the canonical name returned by
    /// `name()` plus a few legacy aliases the CLI used to take from shells
    /// where `+` is awkward to type (`HIGHPLUS`, `HIGH-56`, ...). Single
    /// source of truth for every layer that needs to go from a user- or
    /// wire-supplied string back to a `ProfileIndex`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_uppercase().as_str() {
            "ULTRA" => Some(Self::Ultra),
            "ROBUST" => Some(Self::Robust),
            "NORMAL" => Some(Self::Normal),
            "HIGH" => Some(Self::High),
            "MEGA" => Some(Self::Mega),
            "HIGH+" | "HIGHPLUS" => Some(Self::HighPlus),
            "FAST" => Some(Self::Fast),
            "HIGH++" | "HIGHPLUSPLUS" => Some(Self::HighPlusPlus),
            "HIGH56" | "HIGH-56" => Some(Self::HighFiveSix),
            "HIGH+56" | "HIGHPLUS56" => Some(Self::HighPlusFiveSix),
            _ => None,
        }
    }

    /// All profile indices in canonical order. EXPERIMENTAL profiles
    /// (Mega, Fast, HighPlusPlus, HighFiveSix, HighPlusFiveSix) are
    /// included so they can be selected in forced mode, but they do
    /// NOT take part in auto-detection (cf. `is_experimental`).
    pub const ALL: [Self; 10] = [
        Self::Ultra,
        Self::Robust,
        Self::Normal,
        Self::High,
        Self::Mega,
        Self::HighPlus,
        Self::Fast,
        Self::HighPlusPlus,
        Self::HighFiveSix,
        Self::HighPlusFiveSix,
    ];

    /// `true` for the experimental profiles that must be excluded from
    /// auto-detection (FFT gate). Usable only in RX forced mode.
    ///
    /// Changes 2026-04-28:
    /// - MEGA switched to experimental: theoretical throughput
    ///   (3971 bps) is lower than HIGH (4235) despite FTN tau=30/32
    ///   complexity. HIGH+ (32-APSK validated OTA on HB9MM) replaces
    ///   MEGA in the standard hierarchy.
    /// - HIGH56 promoted to standard: best speed/robustness tradeoff
    ///   on HB9MM (4706 bps with 16-APSK, comfortable SNR margin).
    ///   Becomes the **default** profile GUI-side.
    pub fn is_experimental(self) -> bool {
        matches!(
            self,
            Self::Mega
                | Self::Fast
                | Self::HighPlusPlus
                | Self::HighPlusFiveSix
        )
    }

    /// Preamble family used by this profile on the wire. The split is
    /// driven by `(sps, β)` — profiles that share both end up in the same
    /// family and emit the same preamble sequence ; the header's
    /// `profile_index` byte still disambiguates them downstream.
    pub fn preamble_family(self) -> crate::preamble::PreambleFamily {
        use crate::preamble::PreambleFamily;
        match self {
            // HighPlus / HighPlusPlus / HighFiveSix / HighPlusFiveSix
            // share (sps=32, pitch=32, beta=0.20) with family A -- same
            // preamble as NORMAL/HIGH/MEGA, distinction via mode_code
            // and profile_index.
            Self::Normal
            | Self::High
            | Self::Mega
            | Self::HighPlus
            | Self::HighPlusPlus
            | Self::HighFiveSix
            | Self::HighPlusFiveSix => PreambleFamily::A,
            Self::Robust => PreambleFamily::B,
            Self::Ultra => PreambleFamily::C,
            // Fast has (sps=28, beta=0.15), unique. Reuses the family A
            // QPSK symbols -- the distinction is made by forced mode,
            // not by auto-detection. The RRC is shaping-specific.
            Self::Fast => PreambleFamily::A,
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

/// Resolve a profile name (canonical or legacy alias, see
/// `ProfileIndex::from_name`) to its full `ModemConfig`. The single helper
/// the worker / CLI / Modem trait all funnel through, so adding a new mode
/// only touches `ProfileIndex` itself.
pub fn config_by_name(name: &str) -> Option<ModemConfig> {
    ProfileIndex::from_name(name).map(ProfileIndex::to_config)
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
    /// Multiplier applied to the (otherwise unit-circle) QPSK training
    /// signals -- preamble **and** TDM pilots -- before TX modulation,
    /// and to the matching RX reference (FFE training, pilot residual
    /// computation for sigma^2).
    ///
    /// For nearly every profile this is 1.0; for Apsk64 (HIGH++) we map
    /// the training signals onto the constellation's outer ring R4.
    /// Reason: multiplying the unit QPSK (angles +/-pi/4, +/-3pi/4) by
    /// R4 produces the 4 points that **exactly** match Table 13e
    /// indices 0, 1, 2, 3 (R4 at the same angles). The preamble and
    /// pilots therefore stay a strict subset of the 64-APSK
    /// constellation, and the FFE/LMS adapts on a continuous
    /// preamble<->data<->pilot amplitude -- without the scale "jump"
    /// that prevented the meta CW from decoding and that kept sigma^2
    /// high through inflated pilot residuals (each TDM group =
    /// 2 pilots * 1*R4 mismatch).
    pub fn training_amplitude(&self) -> f64 {
        match self.constellation {
            ConstellationType::Apsk64 => {
                let g1 = self.apsk_gamma;
                let g2 = self.apsk_gamma2;
                let g3 = self.apsk_gamma3;
                // R4 normalised: Es=1 on 4+12+20+28 -> r0^2 = 16/(1+3*g1^2+5*g2^2+7*g3^2),
                // R4 = g3 * r0.
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

    /// Number of LMS training symbols injected between the QPSK
    /// preamble and the header -- an "FFE guard interval" that covers
    /// the 4 rings of the 64-APSK constellation so LMS adapts at the
    /// `mu_train` step before switching to DD.
    ///
    /// For standard profiles: 0 (QPSK preamble alone is enough, the
    /// data constellation decodes at the DD switch). For Apsk64
    /// (HIGH++): 64 (= 4+12+20+28 = a full sweep of the constellation,
    /// each point exactly once). Symbols known on both TX and RX side,
    /// provided by `preamble::make_lms_warmup_for_config`.
    pub fn lms_warmup_syms(&self) -> usize {
        match self.constellation {
            ConstellationType::Apsk64 => 64,
            _ => 0,
        }
    }

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
    ///
    /// The constellation field is only 2 bits; codes >3 (Apsk64=4,
    /// added for HIGH++) are truncated to their low 2 bits (c=4 -> 0,
    /// visual alias of Qpsk in mode_code). This is not a problem
    /// because `profile_index` (a dedicated byte) remains the authority
    /// for reconstructing the ModemConfig RX-side.
    pub fn mode_code(&self) -> u8 {
        let c = self.constellation.code() & 0x03;
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
            apsk_gamma2: 0.0,
            apsk_gamma3: 0.0,
            pilot_pattern: PilotPattern::default_v3(),
        })
    }
}

/// Known symbol rates (index -> rate).
///
/// Rate at idx 7 = `AUDIO_RATE / 28` ~= 1714.286 Bd, used by the
/// experimental FAST profile. Chosen so that sps=28 is integer at 48 kHz.
const SYMBOL_RATES: [(u8, f64); 8] = [
    (0, 500.0),
    (1, 600.0),
    (2, 750.0),
    (3, 1000.0),
    (4, 1200.0),
    (5, 1500.0),
    (6, 2000.0),
    (7, 48_000.0 / 28.0),
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
        apsk_gamma2: 0.0,
        apsk_gamma3: 0.0,
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
        apsk_gamma2: 0.0,
        apsk_gamma3: 0.0,
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
        apsk_gamma2: 0.0,
        apsk_gamma3: 0.0,
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
        apsk_gamma2: 0.0,
        apsk_gamma3: 0.0,
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
        apsk_gamma2: 0.0,
        apsk_gamma3: 0.0,
        pilot_pattern: PilotPattern::dense_ultra(),
    }
}

/// EXPERIMENTAL -- FAST: 16-APSK Rs=48000/28~=1714.3 Bd beta=0.15 LDPC 3/4.
///
/// Differs from HIGH by the (Rs, beta) pair: Rs up to push throughput,
/// beta down to tighten the spectrum and keep the BW within the NBFM
/// plateau. Occupied band at beta=0.15: 1971 Hz centred on 1100 Hz
/// (114-2086 Hz). SPS=28 integer at 48 kHz by construction of the
/// symbol_rate. Gross net: 1714.3 * 4 * 0.75 * (32/34) ~= 4840 bps
/// (vs ~4235 for HIGH).
pub fn profile_fast() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Apsk16,
        ldpc_rate: LdpcRate::R3_4,
        symbol_rate: 48_000.0 / 28.0,
        beta: 0.15,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.85,
        apsk_gamma2: 0.0,
        apsk_gamma3: 0.0,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

/// EXPERIMENTAL -- HIGH+: 32-APSK 1500 Bd beta=0.20 LDPC 3/4.
///
/// Differs from HIGH only by the constellation (16-APSK -> 32-APSK).
/// Everything else is strictly identical to isolate the variable.
/// Radii gamma1=2.84, gamma2=5.27 = DVB-S2 Table 10 EN 302 307-1 for
/// rate 3/4. Gross net: 1500 * 5 * 0.75 * (32/34) ~= 5294 bps
/// (vs ~4235 for HIGH).
pub fn profile_high_plus() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Apsk32,
        ldpc_rate: LdpcRate::R3_4,
        symbol_rate: 1500.0,
        beta: 0.20,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.84,
        apsk_gamma2: 5.27,
        apsk_gamma3: 0.0,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

/// EXPERIMENTAL -- HIGH56: 16-APSK 1500 Bd beta=0.20 **LDPC 5/6**.
///
/// Differs from HIGH only by the LDPC rate (3/4 -> 5/6). Everything
/// else strictly identical to isolate the variable.
/// Gross net: 1500 * 4 * (5/6) * (32/34) ~= 4706 bps (vs ~4235 for HIGH).
/// LDPC margin ~0.7 dB tighter. On HB9MM the channel is well above the
/// threshold for HIGH 3/4, so HIGH56 is probably loss-free decodable
/// -- to validate OTA.
pub fn profile_high_5_6() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Apsk16,
        ldpc_rate: LdpcRate::R5_6,
        symbol_rate: 1500.0,
        beta: 0.20,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.85,
        apsk_gamma2: 0.0,
        apsk_gamma3: 0.0,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

/// EXPERIMENTAL -- HIGH+56: 32-APSK 1500 Bd beta=0.20 **LDPC 5/6**.
///
/// Differs from HIGH+ only by the LDPC rate (3/4 -> 5/6). Everything
/// else strictly identical to isolate the variable.
/// Gross net: 1500 * 5 * (5/6) * (32/34) ~= 5882 bps (vs ~5294 for HIGH+).
/// 32-APSK already tight at 3/4 on HB9MM; HIGH+56 is more marginal and
/// its value will depend on the relay's actual SNR margin.
pub fn profile_high_plus_5_6() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Apsk32,
        ldpc_rate: LdpcRate::R5_6,
        symbol_rate: 1500.0,
        beta: 0.20,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.84,
        apsk_gamma2: 5.27,
        apsk_gamma3: 0.0,
        pilot_pattern: PilotPattern::default_v3(),
    }
}

/// EXPERIMENTAL -- HIGH++: 64-APSK DVB-S2X 1500 Bd beta=0.20 LDPC 3/4.
///
/// Differs from HIGH+ only by the constellation (32-APSK -> 64-APSK
/// 4+12+20+28). Everything else is strictly identical to isolate the
/// variable. Radii gamma1=2.4, gamma2=4.3, gamma3=7.0 =
/// EN 302 307-2 V1.4.1 Table 13f (published for LDPC 11/15; no
/// official set at rate 3/4 for this layout, we reuse the same ratios
/// -- 11/15~=0.733 is very close to 3/4=0.75).
/// Gross net: 1500 * 6 * 0.75 * (32/34) ~= 6353 bps
/// (vs ~5294 for HIGH+). Point density doubled vs HIGH+ -> minimum
/// distance reduced ~sqrt(2): needs a cleaner channel, expect more
/// RaptorQ bursts.
pub fn profile_high_plus_plus() -> ModemConfig {
    ModemConfig {
        constellation: ConstellationType::Apsk64,
        ldpc_rate: LdpcRate::R3_4,
        symbol_rate: 1500.0,
        beta: 0.20,
        tau: 1.0,
        center_freq_hz: DATA_CENTER_HZ,
        apsk_gamma: 2.4,
        apsk_gamma2: 4.3,
        apsk_gamma3: 7.0,
        // Densified pilots (16/2 instead of 32/2): 64-APSK demands a
        // finer phase tracking than 16/32-APSK because the squared
        // min-distance is ~3x smaller. 6% throughput overhead, 2x the
        // intra-segment phase anchoring rate.
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
