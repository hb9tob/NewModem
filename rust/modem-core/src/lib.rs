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
pub mod crc;
pub mod golay;
pub mod header;
pub mod app_header;
pub mod marker;
pub mod payload_envelope;
pub mod profile;
pub mod interleaver;

pub mod ldpc;

pub mod modulator;
pub mod frame;
pub mod raptorq_codec;

pub mod demodulator;
pub mod sync;
pub mod ffe;
pub mod equalizer;
pub mod pll;
pub mod soft_demod;
pub mod rx_v2;
