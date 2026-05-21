//! Shared worker infrastructure used by both [`modem-worker`] (V3) and
//! [`modem-worker2x`] (V4).
//!
//! What lives here is **only** the family-independent surface: the event
//! sink trait, the PTT serial controller, the worker join handle, the
//! tee-to-WAV recording sink. The V3 and V4 state machines themselves
//! sit in their respective sibling crates, each consuming these types.
//!
//! This crate has no DSP dependencies (only `modem-core-base` for
//! `AUDIO_RATE`); the contract is "GUI-visible types and the IO glue
//! every worker needs".
//!
//! [`modem-worker`]: ../modem_worker/index.html
//! [`modem-worker2x`]: ../modem_worker2x/index.html

pub mod event_sink;
pub mod ptt;
pub mod tx_runtime;
pub mod wav_sink;
pub mod worker_handle;

// Channel sounder — TX-side probe-schedule build + RX-side capture
// analyse, designed for two-machine deployments (one operator TX, one
// RX). Wires the `modem-core-base::probe` generators and the matching
// `probe_analyze` measurers into a workflow protected by a wake-up
// tone (VOX/squelch chain) + sync chirp (sample-accurate alignment).
pub mod sounder;

pub use event_sink::{EventSink, EventSinkExt, NoopSink, RecordingSink};
pub use tx_runtime::{
    archive_payload, build_tx_wav_path, run_playback, sanitize_filename, write_tx_wav,
    TxCompleteEvent, TxErrorEvent, TxHandle, TxPlan, TxPlanEvent, TxProgressEvent, TX_VOX_SECONDS,
};
pub use wav_sink::{SharedWavSink, WavSink};
pub use worker_handle::WorkerHandle;
