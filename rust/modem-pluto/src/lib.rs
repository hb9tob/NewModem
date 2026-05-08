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
//!   [`modem_sdr_dsp::fm_demod::QuadratureDemod`],
//!   [`modem_sdr_dsp::decimator::PolyphaseDecimator`] (÷12 → 48 kHz),
//!   [`modem_sdr_dsp::audio_filters::DeemphasisLpf`], then
//!   [`modem_sdr_dsp::audio_filters::SubAudioHpf`].
//!
//! ## Sample-rate strategy
//!
//! Project convention is to pick SDR rates so the ÷N to the modem's
//! locked 48 kHz audio is a **small-prime composite** — no odd and no
//! prime factors above 3. That lets the polyphase decim/interp
//! decompose into cheap multi-stage half-band filters (and matches
//! the AD9361's own internal HB chain, which runs in powers of 2).
//!
//! The driver therefore targets **576 kSa/s = 48 × 12** (12 = 2²·3,
//! all small primes), reaching it via the AD9361's 4× FIR — stock
//! Pluto firmware floors the BBPLL at ~2.083 MS/s without a custom
//! FIR loaded, so the driver writes a 128-tap LPF into
//! `iio:device0/filter_fir_config` and asserts `filter_fir_en = 1` on
//! the per-channel attributes; without that step
//! `sampling_frequency = 576000` fails with `EINVAL`.
//!
//! If 576 refuses to lock, the driver falls back to **2304 kSa/s =
//! 48 × 48** (48 = 2⁴·3 — also clean, doesn't need the FIR-loading
//! dance because it sits above the 2.083 MS/s native floor).
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
//!   negotiates 576 kSa/s (preferred, ratio ÷12) or 2304 kSa/s
//!   (fallback, ratio ÷48) — both clean small-prime composites of 48.
//! * Task #11 (streaming RX/TX through `modem-sdr-dsp` + CLI flags +
//!   loopback test): pending — `rx::start` and the TX submit path
//!   still return `PlutoError::NotImplemented`.

// Cross-platform pieces — these compile everywhere and are the
// long-term home for the Pluto backend. The iiod module is the
// pure-Rust replacement for the libiio C library: TCP transport,
// no FFI, identical wire-protocol on Windows / Linux / Pi.
pub mod error;
pub mod iiod;

// Legacy `industrial-io`-backed modules, gated behind `legacy-iio`
// (default ON on unix, force-OFF on Windows because the upstream FFI
// crate is `#[cfg(unix)]`-only). The migration plan is to port each
// of these to the iiod transport in turn — as that work lands, the
// `cfg` blocks shrink, then the feature goes away.
#[cfg(feature = "legacy-iio")]
pub mod backend;
#[cfg(feature = "legacy-iio")]
pub mod device;
#[cfg(feature = "legacy-iio")]
pub mod rx;
#[cfg(feature = "legacy-iio")]
pub mod sample_sink_adapter;
#[cfg(feature = "legacy-iio")]
pub mod tx;

#[cfg(feature = "legacy-iio")]
pub use backend::{PlutoBackend, PlutoDevice};
pub use error::PlutoError;

/// Default Pluto IIO context URI on this Pi 5 — the device shows up on
/// USB at bus 1 device 6, so `usb:1.6.5` is the typical libiio URI.
/// `ip:pluto.local` is the network-mode equivalent that also works.
pub const DEFAULT_URI: &str = "usb:1.6.5";

/// Preferred AD9363 sample rate, in samples per second. The ratio
/// against the modem's 48 kHz audio rate is **12 = 2²·3** — small
/// primes only, no odd factors, decomposes cleanly into multi-stage
/// half-band filters.
pub const PREFERRED_SAMPLE_RATE_HZ: u64 = 576_000;

/// Fallback AD9363 sample rate if the BBPLL refuses to lock at the
/// preferred rate. The ratio is **48 = 2⁴·3** — also clean, sits
/// above the AD9361's 2.083 MS/s native floor so no FIR loading
/// dance is needed if the chip is being stubborn at 576.
pub const FALLBACK_SAMPLE_RATE_HZ: u64 = 2_304_000;

/// Decimation / interpolation ratio for [`PREFERRED_SAMPLE_RATE_HZ`]
/// against [`modem_sdr_dsp::AUDIO_RATE`].
pub const PREFERRED_RATIO: usize = 12;

/// Decimation / interpolation ratio for [`FALLBACK_SAMPLE_RATE_HZ`].
pub const FALLBACK_RATIO: usize = 48;
