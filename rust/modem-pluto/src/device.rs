//! Pluto device discovery + AD9361 control.
//!
//! Scaffold pass — the real implementation lands in task #10 of the
//! SDR plan. The shape this module will end up with:
//!
//! ```text
//! Context::open(uri)
//!   → find_device("ad9361-phy")            // control plane
//!   → find_device("cf-ad9361-lpc")         // RX buffer device
//!   → find_device("cf-ad9361-dds-core-lpc")// TX buffer device
//!   → load_decimating_fir(...)              // mandatory for 528 kSa/s
//!   → set sampling_frequency = 528_000      // fallback to 960 on EINVAL
//!   → configure freq / gain / rf_bandwidth
//! ```
//!
//! The 4× decimating FIR is the gating piece — without it the BBPLL
//! refuses anything below ~2.083 MS/s and `sampling_frequency = 528000`
//! returns `EINVAL`. The taps are bundled in [`PLUTO_4X_FIR_CONFIG`]
//! so the driver can write them straight into
//! `iio:device0/filter_fir_config` at startup.

use crate::error::PlutoError;

/// Names of the three IIO devices the Pluto exposes that we need.
///
/// `ad9361-phy` is the control-plane device (frequencies, gains,
/// bandwidth, FIR config — no buffer). The other two are the RX and TX
/// buffer-capable devices.
pub mod iio_names {
    pub const PHY: &str = "ad9361-phy";
    pub const RX_BUFFER: &str = "cf-ad9361-lpc";
    pub const TX_BUFFER: &str = "cf-ad9361-dds-core-lpc";
}

/// Static configuration the driver applies after opening a context.
/// The defaults match a typical 2 m amateur-radio NBFM setup; the CLI
/// / GUI override `center_freq_hz` and `rx_gain_db` per session.
#[derive(Clone, Debug)]
pub struct PlutoConfig {
    /// libiio URI, e.g. `"usb:1.6.5"` or `"ip:pluto.local"`.
    pub uri: String,
    /// Tuner LO. Same value used for RX and TX.
    pub center_freq_hz: u64,
    /// Manual RX gain in dB. Range `[-3, 71]` step 1 dB on the AD9363.
    pub rx_gain_db: i32,
    /// TX attenuation in dB (positive = more attenuation). Range
    /// `[-89.75, 0]` dB step 0.25.
    pub tx_attenuation_db: f32,
    /// AD9361 RF bandwidth, in Hz. 200 kHz is comfortable for ±5 kHz
    /// deviation NBFM; the 4× FIR + decimation chain shapes the rest.
    pub rf_bandwidth_hz: u64,
    /// Whether to attempt [`crate::PREFERRED_SAMPLE_RATE_HZ`] first.
    /// Always falls back to [`crate::FALLBACK_SAMPLE_RATE_HZ`] on
    /// `EINVAL`.
    pub prefer_528k: bool,
}

impl Default for PlutoConfig {
    fn default() -> Self {
        Self {
            uri: crate::DEFAULT_URI.to_string(),
            center_freq_hz: 145_500_000,
            rx_gain_db: 30,
            tx_attenuation_db: 10.0,
            rf_bandwidth_hz: 200_000,
            prefer_528k: true,
        }
    }
}

/// Outcome of negotiating a sample rate with the AD9363. The driver
/// picks the ratio that matches the rate that actually locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NegotiatedRate {
    pub sample_rate_hz: u64,
    pub ratio: usize,
}

impl NegotiatedRate {
    pub const PREFERRED: Self = Self {
        sample_rate_hz: crate::PREFERRED_SAMPLE_RATE_HZ,
        ratio: crate::PREFERRED_RATIO,
    };
    pub const FALLBACK: Self = Self {
        sample_rate_hz: crate::FALLBACK_SAMPLE_RATE_HZ,
        ratio: crate::FALLBACK_RATIO,
    };
}

/// Open a libiio context and apply the static parts of `config`.
///
/// **Scaffold**: returns [`PlutoError::NotImplemented`] until task #10
/// lands. The signature is fixed so the RX / TX modules can already
/// import it.
pub fn open(_config: &PlutoConfig) -> Result<PlutoSession, PlutoError> {
    Err(PlutoError::NotImplemented("device::open"))
}

/// Live, fully-configured Pluto session. Owns the libiio `Context` and
/// the three device handles. RX / TX modules borrow buffer devices
/// from here; the phy device handle stays put for runtime gain / freq
/// retuning.
///
/// **Scaffold**: empty today; real fields added in task #10.
#[non_exhaustive]
pub struct PlutoSession {
    pub negotiated_rate: NegotiatedRate,
}

/// Decimating-by-4 FIR config blob, in the textual format
/// `iio:device0/filter_fir_config` expects (one row per tap).
///
/// **Scaffold**: empty placeholder. Task #10 ships the real coefficients
/// — Analog Devices' reference 128-tap LPF for 4× decimation, fc ≈
/// `sample_rate / 4`. The blob is a `&'static str` rather than a file
/// so the driver has zero filesystem dependencies.
pub const PLUTO_4X_FIR_CONFIG: &str = "";
