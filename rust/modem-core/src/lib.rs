//! NBFM audio modem library (V3 wire format).
//!
//! Constellations: QPSK, 8PSK, 16-APSK DVB-S2 (4,12).
//! FEC: LDPC WiMAX IEEE 802.16e, N=2304.
//! Headers: Golay(24,12) coded, always QPSK.
//!
//! Shared DSP primitives (constellation, LDPC, RRC, Golay, FFE, PLL, etc.)
//! live in `modem-core-base` and are re-exported here so existing call-sites
//! `use crate::constellation::...` keep working unchanged.

// --- Shared DSP, re-exported from modem-core-base (V3 + 2x share these) ---
pub use modem_core_base::{
    constellation,
    demodulator,
    equalizer,
    ffe,
    golay,
    interleaver,
    ldpc,
    modulator,
    pll,
    rrc,
    soft_demod,
    sync,
    traits,
    types,
};

// --- V3-specific modules (frame format, RX pipeline, V3 profile decorations) ---
pub mod preamble;
pub mod pilot;
pub mod header;
pub mod marker;
pub mod profile;
pub mod frame;
pub mod rx_v2;
pub mod gate;
pub mod v3_modem;
