//! GUI-agnostic worker layer for the NBFM modem.
//!
//! This crate hosts the TX and RX orchestration that used to live inside
//! `modem-gui/src-tauri`. The point of the extraction is twofold:
//!
//! 1. The Tauri application becomes a thin shim: receive Tauri commands,
//!    construct workers with a Tauri-flavored event sink, forward back.
//! 2. Other front-ends (a CLI auto-receiver, a future TUI, integration
//!    tests) can drive the same workers with a different sink — for
//!    instance one that logs JSON lines to stdout.
//!
//! In phase 2A only the `event_sink` module is populated. The rx/tx
//! workers themselves move in phases 2B and 2C; phase 2D drops the
//! subprocess dependency for TX.

pub mod event_sink;

pub use event_sink::{EventSink, EventSinkExt, NoopSink, RecordingSink};
