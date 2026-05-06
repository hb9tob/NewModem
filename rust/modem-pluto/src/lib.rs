//! PlutoSDR backend for the NBFM modem.
//!
//! ADALM-PLUTO (AD9363, libiio firmware ≥ 0.31 / "tezuka" branch) wired
//! into the same seams the cpal soundcard backend uses:
//!
//! * **TX**: [`tx::PlutoSink`] implements [`modem_io::traits::SampleSink`] —
//!   the worker hands a 48 kHz mono `Vec<f32>` over and the sink runs it
//!   through [`modem_sdr_dsp::interpolator::PolyphaseInterpolator`] then
//!   [`modem_sdr_dsp::pm_mod::PhaseMod`] (radio-faithful PM with the
//!   built-in +6 dB/oct preemphasis), packs the I/Q to S16LE, and pushes
//!   buffers into `cf-ad9361-dds-core-lpc` over libiio.
//! * **RX**: [`rx::start`] returns `(CaptureHandle, Receiver<Vec<f32>>)`,
//!   the same shape as `modem_io::cpal_capture::start`. The capture
//!   thread reads S12-in-S16 from `cf-ad9361-lpc`, runs
//!   [`modem_sdr_dsp::decimator::PolyphaseDecimator`] (×11 → 48 kHz),
//!   [`modem_sdr_dsp::fm_demod::QuadratureDemod`],
//!   [`modem_sdr_dsp::audio_filters::DeemphasisLpf`], then
//!   [`modem_sdr_dsp::audio_filters::SubAudioHpf`].
//!
//! ## Sample-rate strategy
//!
//! AD9363 BBPLL on stock Pluto firmware bottoms out at ~520 kSa/s, so
//! the driver targets **528 kSa/s = 48 × 11** (the lowest rate that
//! gives an integer ÷N to the modem's locked 48 kHz). Reaching 528
//! requires loading a 4× decimating FIR into `iio:device0/filter_fir_config`
//! and asserting `filter_fir_en = 1` on the relevant per-channel
//! attributes — without that step `sampling_frequency = 528000` fails
//! with `EINVAL`. If 528 still refuses to lock, the driver falls back
//! to **960 kSa/s = 48 × 20**, which the AD9363 takes natively.
//!
//! ## Status
//!
//! * Task #9 (scaffold): done.
//! * Task #10 (device discovery + FIR loading): [`device::open`] is
//!   live — it opens the libiio context, locates the three IIO
//!   devices, programs gain / freq / bandwidth, computes a 128-tap
//!   Hamming-windowed sinc FIR via [`device::design_decim4_lpf`],
//!   formats it with [`device::format_fir_blob`], writes it into
//!   `filter_fir_config`, enables per-direction `filter_fir_en`, and
//!   negotiates 528 kSa/s (preferred) or 960 kSa/s (fallback).
//! * Task #11 (streaming RX/TX through `modem-sdr-dsp` + CLI flags +
//!   loopback test): pending — `rx::start` and the TX submit path
//!   still return `PlutoError::NotImplemented`.

pub mod device;
pub mod error;
pub mod rx;
pub mod tx;

pub use error::PlutoError;

/// Default Pluto IIO context URI on this Pi 5 — the device shows up on
/// USB at bus 1 device 6, so `usb:1.6.5` is the typical libiio URI.
/// `ip:pluto.local` is the network-mode equivalent that also works.
pub const DEFAULT_URI: &str = "usb:1.6.5";

/// Preferred AD9363 sample rate, in samples per second. Decimation
/// ratio against the modem's 48 kHz audio rate is 11.
pub const PREFERRED_SAMPLE_RATE_HZ: u64 = 528_000;

/// Fallback AD9363 sample rate if the BBPLL refuses to lock at the
/// preferred rate. Decimation ratio is 20.
pub const FALLBACK_SAMPLE_RATE_HZ: u64 = 960_000;

/// Decimation / interpolation ratio for [`PREFERRED_SAMPLE_RATE_HZ`]
/// against [`modem_sdr_dsp::AUDIO_RATE`].
pub const PREFERRED_RATIO: usize = 11;

/// Decimation / interpolation ratio for [`FALLBACK_SAMPLE_RATE_HZ`].
pub const FALLBACK_RATIO: usize = 20;
