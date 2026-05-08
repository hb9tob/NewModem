//! SDRplay RSPduo backend for the NBFM modem.
//!
//! Wraps the closed-binary SDRplay API 3.x (libsdrplay_api.so + the
//! sdrplay_apiService daemon) into the same seam as the Pluto and
//! cpal backends:
//!
//! * **RX**: [`rx::start`] returns
//!   `(CaptureHandle, mpsc::Receiver<Vec<f32>>)` — a 48 kHz mono
//!   `Vec<f32>` stream identical in shape to
//!   `modem_io::cpal_capture::start` and `modem_pluto::rx::start`.
//!   The capture chain runs the radio-faithful demod from
//!   `modem-sdr-dsp` (QuadratureDemod → PolyphaseDecimator →
//!   DeemphasisLpf → SubAudioHpf), so `rx_worker` is agnostic to
//!   which SDR fed it.
//! * **TX**: out of scope — RSPduo is RX-only hardware.
//!
//! ## Setup (one-time, user-side)
//!
//! The SDRplay API isn't redistributable. Install it from
//! <https://www.sdrplay.com/api/> (free, account-walled):
//!
//! ```text
//! chmod +x SDRplay_RSP_API-Linux-ARM64-3.X.run
//! sudo ./SDRplay_RSP_API-Linux-ARM64-3.X.run
//! sudo systemctl enable --now sdrplay
//! ```
//!
//! That drops `libsdrplay_api.so` under `/usr/local/lib`, the C
//! headers under `/usr/local/include`, the daemon binary under
//! `/opt/sdrplay_api/`, and the udev rule that lets non-root see
//! the RSPduo's USB descriptor. Verify with
//! `systemctl status sdrplay`. Build-time overrides:
//! `SDRPLAY_API_INCLUDE_DIR` and `SDRPLAY_API_LIB_DIR`.
//!
//! ## Sample-rate strategy
//!
//! Same convention as `modem_pluto`: pick rates that divide cleanly
//! to 48 kHz with small-prime composites only. The RSPduo's master
//! clock supports a wide range; we target [`PREFERRED_SAMPLE_RATE_HZ`]
//! = 2.304 MS/s with the API's internal `decimation = 4` so the host
//! receives 576 kSa/s — **48 × 48000 = 2_304_000** (an even multiple
//! of 48 kHz, factors 2⁴·3·...·48 = 2⁴·3 — small primes only), then
//! ÷12 in our DSP to land at 48 kHz audio. Both ratios (`fs / 48000`
//! at 48, and the host-rate ÷12) are even — what the project's
//! sample-rate convention asks for.
//!
//! ## Status
//!
//! Phase 1 (this commit): driver layer + bindgen + RX path with the
//! callback → DSP chain → mpsc plumbing. GUI integration to follow
//! in a separate commit so the rx_worker's existing routing on
//! `pluto:` prefix gets a `sdrplay:` sibling.

pub mod api;
pub mod backend;
pub mod device;
pub mod error;
pub mod rx;

pub use backend::{SdrplayBackend, SdrplayDevice};
pub use device::{
    list_serials, open, AgcMode, AntennaPort, SdrplayConfig, SdrplaySession, Tuner,
    PREFERRED_AUDIO_RATIO, PREFERRED_DECIMATION, PREFERRED_SAMPLE_RATE_HZ,
};
pub use error::SdrplayError;
