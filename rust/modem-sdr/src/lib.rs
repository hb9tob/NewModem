//! Backend-agnostic SDR trait surface for the NBFM modem.
//!
//! This crate defines the contract that every SDR backend implements
//! (`PlutoBackend`, `SdrplayBackend`, future `RtlsdrBackend`, …) so
//! the GUI never `use`s a backend by name. The Tauri layer talks to
//! `&dyn SdrBackend` and `Box<dyn SdrDevice>`; controls in the
//! frontend are constructed from a serialized
//! [`BackendCapabilities`] descriptor.
//!
//! Layering — no cycles:
//!
//! ```text
//!   modem-gui   ── ne use aucun backend nommé
//!         │
//!         ▼
//!   modem-sdr  (this crate; pure traits, no optional backend deps)
//!         ▲
//!         │ impl
//!   ┌─────┴───────┬────────────────┬──────────────┐
//!   │ modem-pluto │ modem-sdrplay  │ modem-rtlsdr │ (futur)
//!   └──────┬──────┴────────┬───────┴──────────────┘
//!          │               │
//!          └─ modem-sdr-dsp (decim / demod / emphasis / CTCSS)
//! ```
//!
//! The registry that turns this into a runtime list of compiled-in
//! backends lives in `modem-gui/src-tauri/src/sdr_registry.rs`,
//! gated by per-backend cargo features. Putting it there (not here)
//! keeps `modem-sdr` cycle-free w.r.t. the backend crates.

pub mod capabilities;
pub mod config;
pub mod device;
pub mod error;
pub mod handle;
pub mod traits;

pub use capabilities::{
    AgcMode, AntennaChoice, BackendCapabilities, BackendFeatures, ManualGainShape,
    SampleRateStrategy, TunerOption,
};
pub use config::{GainSetting, ManualGainValue, SdrConfig};
pub use device::DeviceDescriptor;
pub use error::SdrError;
pub use handle::SdrCaptureHandle;
pub use traits::{SdrBackend, SdrDevice};
