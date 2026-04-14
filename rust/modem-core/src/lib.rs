//! Modem NBFM single-carrier shaped PSK/QAM avec pilotes TDM.
//!
//! Architecture :
//!  bytes -> [optionnel: RS encode] -> [optionnel: LDPC encode]
//!        -> bit-to-symbol mapping (8PSK/16QAM/32QAM)
//!        -> insertion pilotes TDM
//!        -> upsampling + RRC pulse shaping
//!        -> upmix to passband (audio)
//!
//! Cette premiere version expose juste le modulateur sans FEC.

pub mod constellation;
pub mod framing;
pub mod modulator;
pub mod rrc;

pub const AUDIO_RATE: u32 = 48_000;
pub const DATA_CENTER_HZ: f32 = 1100.0;
pub const ROLLOFF: f32 = 0.25;

/// Parametres TDM : nombre de symboles data + nombre de pilotes par groupe
pub const D_SYMS: usize = 32;
pub const P_SYMS: usize = 2;

/// Parametres preambule QPSK pour synchronisation timing
pub const N_PREAMBLE_SYMBOLS: usize = 256;

/// Modes modem disponibles
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModemMode {
    /// 8PSK Gray, 1500 Bd, 1/2 LDPC (mode robuste 9-13 dB SNR)
    Psk8R12_1500,
    /// 16QAM, 1600 Bd, 1/2 LDPC (mode 13-22 dB)
    Qam16R12_1600,
    /// 16QAM, 1500 Bd, 3/4 LDPC (mode rapide 22+ dB)
    Qam16R34_1500,
    /// 32QAM cross, 1200 Bd, 3/4 LDPC (mode rapide alternatif)
    Qam32R34_1200,
    /// 8PSK 500 Bd 1/2 LDPC (mode survie 4-9 dB)
    Psk8R12_500,
}

impl ModemMode {
    pub fn symbol_rate(&self) -> u32 {
        match self {
            ModemMode::Psk8R12_1500 | ModemMode::Qam16R34_1500 => 1500,
            ModemMode::Qam16R12_1600 => 1600,
            ModemMode::Qam32R34_1200 => 1200,
            ModemMode::Psk8R12_500 => 500,
        }
    }

    pub fn bits_per_symbol(&self) -> usize {
        match self {
            ModemMode::Psk8R12_1500 | ModemMode::Psk8R12_500 => 3,
            ModemMode::Qam16R12_1600 | ModemMode::Qam16R34_1500 => 4,
            ModemMode::Qam32R34_1200 => 5,
        }
    }

    /// Numerateur du code rate (1 pour 1/2, 3 pour 3/4)
    pub fn fec_num(&self) -> usize {
        match self {
            ModemMode::Psk8R12_1500
            | ModemMode::Psk8R12_500
            | ModemMode::Qam16R12_1600 => 1,
            ModemMode::Qam16R34_1500 | ModemMode::Qam32R34_1200 => 3,
        }
    }

    pub fn fec_den(&self) -> usize {
        match self {
            ModemMode::Psk8R12_1500
            | ModemMode::Psk8R12_500
            | ModemMode::Qam16R12_1600 => 2,
            ModemMode::Qam16R34_1500 | ModemMode::Qam32R34_1200 => 4,
        }
    }

    /// Debit net en bits/s (avec FEC + overhead pilotes TDM)
    pub fn net_bps(&self) -> u32 {
        let pilots_eff = D_SYMS as f32 / (D_SYMS + P_SYMS) as f32;
        let fec_rate = self.fec_num() as f32 / self.fec_den() as f32;
        (self.symbol_rate() as f32
            * self.bits_per_symbol() as f32
            * fec_rate
            * pilots_eff) as u32
    }

    /// Parse depuis chaine "8PSK-1/2-1500", "16QAM-3/4-1500", etc.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "8PSK-1/2-1500" => Some(ModemMode::Psk8R12_1500),
            "8PSK-1/2-500" => Some(ModemMode::Psk8R12_500),
            "16QAM-1/2-1600" => Some(ModemMode::Qam16R12_1600),
            "16QAM-3/4-1500" => Some(ModemMode::Qam16R34_1500),
            "32QAM-3/4-1200" => Some(ModemMode::Qam32R34_1200),
            _ => None,
        }
    }
}
