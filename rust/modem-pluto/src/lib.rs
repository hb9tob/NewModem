//! PlutoSDR backend for the NBFM modem.
//!
//! ADALM-PLUTO (AD9363) wired into the same seams the cpal soundcard
//! backend uses:
//!
//! * **TX**: [`tx::PlutoSink`] implements [`modem_io::traits::SampleSink`] —
//!   the worker hands a 48 kHz mono `Vec<f32>` over and the sink runs it
//!   through [`modem_sdr_dsp::interpolator::PolyphaseInterpolator`] then
//!   [`modem_sdr_dsp::pm_mod::PhaseMod`] (radio-faithful PM with the
//!   built-in +6 dB/oct preemphasis), packs the I/Q to S16LE, and pushes
//!   buffers into `cf-ad9361-dds-core-lpc` via iiod WRITEBUF.
//! * **RX**: [`rx::start`] returns `(CaptureHandle, Receiver<Vec<f32>>)`,
//!   the same shape as `modem_io::cpal_capture::start`. The capture
//!   thread reads S12-in-S16 from `cf-ad9361-lpc` via iiod READBUF,
//!   runs [`modem_sdr_dsp::fm_demod::QuadratureDemod`],
//!   [`modem_sdr_dsp::decimator::PolyphaseDecimator`] (÷12 → 48 kHz),
//!   [`modem_sdr_dsp::audio_filters::DeemphasisLpf`], then
//!   [`modem_sdr_dsp::audio_filters::SubAudioHpf`].
//!
//! ## Pure-Rust transport (no libiio FFI)
//!
//! The chip is reached via the `iiod` TCP protocol that ships in the
//! Pluto firmware itself. Pluto exposes iiod on port 30431 over
//! USB-NCM (192.168.2.1 by default — assigned by the AD USB driver
//! on Windows or by the cdc-ncm kernel module on Linux), or any IP
//! the user specifies. One code path on every host OS, no
//! `libiio.dll` to bundle, no kernel-driver dance — USB-NCM is a
//! standard Windows class, signed by Microsoft. See [`iiod`] for the
//! transport details and [`iiod::target::parse_pluto_target`] for
//! the URI grammar.
//!
//! ## Sample-rate strategy
//!
//! Project convention is to pick SDR rates so the ÷N to the modem's
//! locked 48 kHz audio is a **small-prime composite** — no odd and no
//! prime factors above 3. That lets the polyphase decim/interp
//! decompose into cheap multi-stage half-band filters (and matches
//! the AD9361's own internal HB chain, which runs in powers of 2).
//!
//! The driver targets **576 kSa/s = 48 × 12** (12 = 2²·3, all small
//! primes), reaching it via the AD9361's 4× FIR — stock Pluto
//! firmware floors the BBPLL at ~2.083 MS/s without a custom FIR
//! loaded, so the driver writes a 128-tap LPF into
//! `iio:device0/filter_fir_config` and asserts `filter_fir_en = 1` on
//! the per-channel attributes; without that step
//! `sampling_frequency = 576000` fails with `EINVAL`.
//!
//! If 576 refuses to lock, the driver falls back to **2304 kSa/s =
//! 48 × 48** (48 = 2⁴·3 — also clean, doesn't need the FIR-loading
//! dance because it sits above the 2.083 MS/s native floor).

pub mod backend;
pub mod device;
pub mod error;
pub mod iiod;
pub mod rx;
pub mod sample_sink_adapter;
pub mod tx;

pub use backend::{PlutoBackend, PlutoDevice};
pub use error::PlutoError;

/// Default Pluto iiod URI. Pluto over USB exposes itself as a
/// USB-NCM virtual NIC at this address — works identically on
/// Windows (AD USB driver) and Linux (cdc-ncm kernel module). For
/// network-mode (Pluto+ on Ethernet, or any IP-reachable Pluto), the
/// GUI / CLI passes a custom `ip:host[:port]` instead.
pub const DEFAULT_URI: &str = "ip:192.168.2.1";

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
