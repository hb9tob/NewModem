//! RTL-SDR backend for the NBFM modem.
//!
//! Supports the **RTL-SDR Blog V3** (R820T2 tuner) and **V4** (R828D
//! tuner) — both use the same `librtlsdr` ABI from the rtl-sdr-blog
//! fork (≥ v2.0). Anything else built on the RTL2832U with an R820T-
//! family tuner should also work; the backend reads the live USB
//! strings and tuner type to label the device rather than gating on a
//! specific model.
//!
//! Wraps `librtlsdr` into the same seam as the Pluto, SDRplay and cpal
//! backends:
//!
//! * **RX**: [`rx::start`] returns
//!   `(CaptureHandle, mpsc::Receiver<Vec<f32>>)` — a 48 kHz mono
//!   `Vec<f32>` stream identical in shape to
//!   `modem_io::cpal_capture::start`, `modem_pluto::rx::start`, and
//!   `modem_sdrplay::rx::start`. The capture thread runs the
//!   radio-faithful demod from `modem-sdr-dsp` (FreqXlatingFir at
//!   `+250 kHz` LO offset → QuadratureDemod → DeemphasisLpf →
//!   SubAudioHpf), so `rx_worker` is agnostic to which radio fed it.
//! * **TX**: out of scope — the RTL2832U is an RX-only ADC.
//!
//! ## Single-binary deployment story
//!
//! `librtlsdr.so` is loaded **at runtime** via the [`libloading`]
//! crate; the GUI binary has no ELF NEEDED entry for it. On a host
//! without librtlsdr installed:
//!
//! * [`backend::RtlsdrBackend::list_devices`] returns `Ok(Vec::new())`
//!   (empty, not an error) so the GUI device dropdown simply doesn't
//!   show any RTL-SDR row.
//! * [`backend::RtlsdrBackend::library_available`] returns `false` so
//!   the Paramètres panel paints a "Bibliothèque manquante" inline
//!   status next to the "Activer RTL-SDR" checkbox.
//!
//! This lets a single `.deb` install on machines with or without
//! `librtlsdr0`. The package declares `Recommends: librtlsdr0`, so
//! `apt` pulls it in by default but the package installs cleanly
//! without it.
//!
//! ## RTL-SDR-specific keys recognised in [`modem_sdr::SdrConfig::backend_extras`]
//!
//! - `ppm_correction: i32` — crystal-error correction in ppm
//!   (`rtlsdr_set_freq_correction`). Default `0`.
//! - `direct_sampling: bool` — RTL-SDR Blog V3 HF mode (Q-branch).
//!   Default `false`. Ignored on V4 (which uses its internal
//!   upconverter for HF — out of scope for this initial drop).
//! - `tuner_bandwidth_hz: u64` — when set, requests an analogue
//!   tuner BW (`rtlsdr_set_tuner_bandwidth`). `0` or absent = let
//!   the tuner pick its default (~2 MHz for the R820T family).
//!
//! Unknown keys are silently ignored.
//!
//! ## Sample-rate strategy
//!
//! 2 304 000 Sa/s u8 I/Q from the dongle → `÷48` polyphase decimator
//! in `NbfmRxChain` → 48 kHz audio. **48 × 48000 = 2_304_000** keeps
//! the decimation small-prime (2⁴·3) and the tap budget identical to
//! Pluto's BBPLL-fallback path (already DSP-tested by
//! `nbfm_rx_chain::pluto_fallback_rate_works`).

pub mod backend;
pub mod device;
pub mod error;
pub mod ffi;
pub mod rx;

pub use backend::{RtlsdrBackend, RtlsdrDevice, BACKEND_ID};
pub use device::{
    list_devices_meta, open, RtlsdrConfig, RtlsdrHardware, RtlsdrSession,
    DEFAULT_LO_OFFSET_HZ, GAIN_TABLE_TENTHS_DB, PREFERRED_SAMPLE_RATE_HZ,
};
pub use error::RtlsdrError;
