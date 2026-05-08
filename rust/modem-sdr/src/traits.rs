//! Backend-agnostic SDR traits.
//!
//! Two traits, one per layer:
//!   - [`SdrBackend`] describes a *family* of devices (Pluto, SDRplay,
//!     RTL-SDR, …). A zero-sized type per crate (`PlutoBackend`,
//!     `SdrplayBackend`) lives in the registry and answers questions
//!     about what the family can do (`capabilities`), what's plugged
//!     in right now (`list_devices`), and how to open one (`open`).
//!   - [`SdrDevice`] is one *opened* device. The runtime contract:
//!     hand it a config at open() time, then ask it to `start_rx()`
//!     for samples or call `tx_sink()` for a TX `SampleSink`. Live
//!     retune (`update_*`) is scaffolded but Phase-1 impls all return
//!     [`SdrError::NotImplemented`].
//!
//! `SdrDevice: Send` (not `Sync`). The GUI stores one inside
//! `Mutex<Option<CaptureSession>>` — the mutex enforces single-thread
//! access, so `Sync` is unnecessary. Allowing `!Sync` keeps SDRplay's
//! raw `*mut sdrplay_api_DeviceParamsT` ergonomic without per-backend
//! `unsafe impl Sync`.

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::capabilities::{AntennaChoice, BackendCapabilities};
use crate::config::{GainSetting, SdrConfig};
use crate::device::DeviceDescriptor;
use crate::error::SdrError;
use crate::handle::SdrCaptureHandle;

/// Family-level operations: enumerate, describe, open.
pub trait SdrBackend: Send + Sync {
    /// Stable ASCII identifier — also the dropdown grouping key, the
    /// MRU-favourites bucket key, and the prefix in
    /// `<backend_id>:<device_id>` composite names.
    /// Examples: `"pluto"`, `"sdrplay"`, `"rtlsdr"`.
    fn id(&self) -> &'static str;

    /// Human-friendly label, e.g. `"PlutoSDR (ADALM-PLUTO)"`,
    /// `"SDRplay RSPduo"`. The GUI shows this as a `<optgroup>`
    /// label in the device dropdown.
    fn display_name(&self) -> &'static str;

    /// Static capabilities of this backend family. Returned by
    /// reference because backends typically own a `OnceLock`-cached
    /// instance — no allocation per call.
    fn capabilities(&self) -> &BackendCapabilities;

    /// Live device enumeration. Empty `Vec` + `Ok(())` means
    /// "the backend works but no device is plugged in" — the GUI
    /// surfaces it as an empty group, not an error banner.
    fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, SdrError>;

    /// Open the device described by `descriptor.id` with `config`.
    /// The backend validates `config` against its
    /// `capabilities()` and returns
    /// [`SdrError::InvalidConfig`] on mismatch.
    fn open(
        &self,
        descriptor: &DeviceDescriptor,
        config: &SdrConfig,
    ) -> Result<Box<dyn SdrDevice>, SdrError>;
}

/// One opened SDR device. Owns the hardware until dropped.
pub trait SdrDevice: Send {
    /// Echo back the descriptor that was used to open this device,
    /// so the GUI can label the running session.
    fn descriptor(&self) -> &DeviceDescriptor;

    /// Echo back the config currently driving the device. Caller
    /// can diff against new settings to decide between a hot
    /// `update_*` call and a full close-reopen cycle.
    fn config(&self) -> &SdrConfig;

    /// Capabilities of *this instance*. Default: same as the
    /// backend's. A backend may override (e.g. a tuner whose
    /// frequency lock is narrower than the chip's claim).
    fn capabilities(&self) -> &BackendCapabilities;

    /// Start a 48 kHz mono f32 RX capture. Returns a teardown
    /// handle (drop = stop) and an mpsc Receiver — same shape as
    /// `modem_io::cpal_capture::start`, so `rx_worker::spawn`
    /// plugs in unchanged.
    ///
    /// Backends that are TX-only or RX-disabled return
    /// [`SdrError::NotSupported`].
    fn start_rx(
        &mut self,
    ) -> Result<(SdrCaptureHandle, Receiver<Vec<f32>>), SdrError>;

    /// A `SampleSink` for TX, or `None` if this device is RX-only.
    /// `tx_worker` polls for `is_done` after `play_buffer(...)`.
    fn tx_sink(&self) -> Option<Arc<dyn modem_io::SampleSink>>;

    // ---- live-retune scaffolding ----
    //
    // Phase-1 impls return [`SdrError::NotImplemented`]. The GUI
    // surfaces those as a tooltip "feature not yet wired up" rather
    // than a red banner. Hooking them up to `sdrplay_api_Update` /
    // Pluto attribute writes is a follow-up PR.

    fn update_rx_freq(&mut self, hz: u64) -> Result<(), SdrError>;
    fn update_tx_freq(&mut self, hz: u64) -> Result<(), SdrError>;
    fn update_gain(&mut self, gain: GainSetting) -> Result<(), SdrError>;
    fn update_antenna(&mut self, choice: AntennaChoice) -> Result<(), SdrError>;
}
