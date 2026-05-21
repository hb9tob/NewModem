//! Shared DSP primitives for NewModem modems.
//!
//! Started life as the shared base between the V3 (`modem-core`) and the 2x
//! (`modem-core2x`) PHYs. The 2x track was retired on the `feat/v3-turbo`
//! branch; `modem-core` is now the only consumer, but the crate is kept
//! separate so the sounder (`modem-worker-base/src/sounder.rs`) and any
//! future RX work (turbo Pass 2 / closed-loop timing) can pull primitives
//! from one place without cycling through `modem-core`.
//!
//! Contents: constellations (DVB-S2/S2X), LDPC WiMAX, RRC, Golay(24,12), the
//! feed-forward equaliser, the decision-directed PLL, soft demodulation, the
//! Farrow interpolator + timing loop scaffolding, the G3RUH scrambler, the
//! probe generators / analysers for the channel sounder, and the
//! cross-cutting `Modem` trait. No frame-format-specific logic lives here.

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

// Phase B — closed-loop Gardner TED + PI loop filter. Pairs with
// `farrow` to drive a continuous timing-recovery strobe. Standard
// Gardner for PSK + AbsGardner for APSK constellations.
pub mod timing_loop;

// Channel sounder — probe-signal generators (tone, two-tone, chirp,
// multitone, AWGN, level sweep) for characterising the radio chain
// (transceiver + soundcard + SDR). The matching analyser lives in
// `probe_analyze`. Both modules are pure functions; the TX
// orchestration (PTT, soundcard playback, raw RX capture) is wired in
// `modem-worker-base/src/sounder.rs`.
pub mod probe;
pub mod probe_analyze;

// G3RUH self-synchronising scrambler / descrambler (multiplicative,
// G(x) = 1 + x^12 + x^17). Applied on the source payload before
// RaptorQ at TX and on the reassembled payload after RaptorQ at RX, to
// whiten the bitstream regardless of user content.
pub mod scrambler;
