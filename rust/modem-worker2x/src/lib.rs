//! GUI-agnostic TX/RX orchestration for the 2x (V4) family.
//!
//! Sibling of `modem-worker` (V3). The two crates are deliberately
//! parallel rather than unified so the legacy V3 path stays bit-for-bit
//! unchanged while 2x evolves; future factoring into a shared
//! `modem-worker-base` is left for a follow-up once 2x stabilises.
//!
//! Modules:
//!
//! - [`tx_worker2x`] — encode an in-process payload to `Vec<f32>` audio
//!   via [`modem_core2x::modem2x::V4Modem`]. Optionally drives a
//!   [`SampleSink`](modem_io::SampleSink) for live transmission.
//! - [`rx_worker2x`] — convert audio f32 samples (mono, 48 kHz) into
//!   complex symbols via downmix + matched filter, then call
//!   [`modem_core2x::rx_v4::rx_v4_symbols`]. The TimingLoop / Farrow
//!   integration is a future enhancement; this first cut uses naive
//!   integer-step sampling at the symbol rate which is sufficient for
//!   noise-free WAV roundtrips and OTA captures with negligible clock
//!   skew (Pi5 + sound card baseline).
//! - [`session_store2x`] — accumulates decoded payloads keyed by
//!   `session_id`. Emits the recovered file once an [`AppHeader`] is
//!   seen and enough RaptorQ packets have converged.
//!
//! [`AppHeader`]: modem_framing::app_header::AppHeader
//!
//! `ptt`, `EventSink`, `WorkerHandle` and `WavSink` are re-exported from
//! [`modem_worker_base`] so the 2x worker doesn't duplicate the GPIO /
//! serial layer or the event abstraction. The V3 worker
//! (`modem-worker`) and the V4 worker (here) both sit on top of the
//! same shared infrastructure.

pub mod rx_worker2x;
pub mod session_store2x;
pub mod tx_worker2x;

pub use modem_worker_base::ptt;
pub use modem_worker_base::{
    EventSink, EventSinkExt, NoopSink, RecordingSink, SharedWavSink, WavSink, WorkerHandle,
};
