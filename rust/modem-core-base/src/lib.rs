//! Shared DSP primitives for NewModem modems.
//!
//! Both the V3 (`modem-core`) and the 2x (`modem-core2x`) PHYs depend on this
//! crate for: constellations (DVB-S2/S2X), LDPC WiMAX, RRC, Golay(24,12), the
//! feed-forward equaliser, the decision-directed PLL, soft demodulation, and
//! the cross-cutting `Modem` trait. No frame-format-specific logic lives here.

pub mod types;
pub mod profile_types;
pub mod constellation;
pub mod rrc;
pub mod golay;
pub mod interleaver;
pub mod ldpc;
pub mod modulator;
pub mod demodulator;
pub mod sync;
pub mod ffe;
pub mod equalizer;
pub mod pll;
pub mod soft_demod;
pub mod traits;

// Phase A — Farrow cubic-Lagrange interpolator (DVB-S2X-style continuous
// timing recovery building block). Isolated module, no integration into
// V3 yet; consumed by the upcoming Phase B closed-loop Gardner and by
// the 2x RX pipeline.
pub mod farrow;
