//! NBFM audio modem library.
//!
//! Constellations: QPSK, 8PSK, 16-APSK DVB-S2 (4,12).
//! FEC: LDPC WiMAX IEEE 802.16e, N=2304.
//! Headers: Golay(24,12) coded, always QPSK.

pub mod types;
pub mod constellation;
pub mod rrc;
pub mod preamble;
pub mod pilot;
pub mod golay;
pub mod header;
pub mod marker;
pub mod profile;
pub mod interleaver;

pub mod ldpc;

pub mod modulator;
pub mod frame;

// Transport-agnostic framing modules now live in `modem-framing`. The
// re-exports keep every existing call site (modem-cli, modem-worker,
// modem-gui, tests) compiling without modification — they will be
// retired in a follow-up cleanup phase once all consumers migrate to
// the direct `modem_framing::*` import path. `crc` moves with them
// because `app_header::crc16` requires it and modem-framing must not
// depend on modem-core (would create a cycle).
pub use modem_framing::{app_header, crc, payload_envelope, raptorq_codec};

pub mod demodulator;
pub mod sync;
pub mod ffe;
pub mod equalizer;
pub mod pll;
pub mod soft_demod;
pub mod rx_v2;
pub mod gate;

pub mod traits;
pub mod v3_modem;
