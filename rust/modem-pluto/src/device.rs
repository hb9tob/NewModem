//! Pluto device discovery + AD9361/AD9363 control.
//!
//! Opens a libiio context, finds the three IIO devices the AD9363 ASoC
//! driver exposes, loads a 4× decimating/interpolating FIR into
//! `filter_fir_config`, and programs the requested sample rate, LO,
//! RX gain, TX attenuation, and RF bandwidth.
//!
//! The 4× FIR is the gating piece. Stock Pluto firmware exposes a
//! BBPLL minimum of 2.083 MS/s without a custom FIR loaded, which is
//! why `sampling_frequency_available` reads `[2083333 1 61440000]` on
//! a fresh context. Loading a 4× decimating FIR effectively divides
//! that floor by 4, so the chip then accepts 576 kSa/s
//! ([`crate::PREFERRED_SAMPLE_RATE_HZ`], chosen so the ÷N to 48 kHz
//! audio is the small-prime composite 12 = 2²·3 — see project memory
//! `sdr-rate-convention.md`).
//!
//! The AD9361 driver expects the FIR config in a small ASCII blob
//! ([`format_fir_blob`]) written into the device-attribute
//! `filter_fir_config` on `ad9361-phy`. The taps are computed at
//! startup with [`design_decim4_lpf`] — a 128-tap Hamming-windowed
//! sinc — so the crate stays runtime-only and ships no opaque tap
//! files. See the constants in that helper for the design choices.

use industrial_io::{Channel, Context, Device, Direction};

use crate::error::PlutoError;

/// Names of the three IIO devices the Pluto exposes that we drive.
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
/// / GUI override the freq + gain fields per session.
///
/// RX and TX have **independent** LOs on the AD9361 (`altvoltage0` =
/// RX_LO, `altvoltage1` = TX_LO). Set them to the same value for
/// simplex; offset them for repeater duplex (e.g. RX 145.625 MHz /
/// TX 145.025 MHz on a 2 m repeater with a -600 kHz shift).
#[derive(Clone, Debug)]
pub struct PlutoConfig {
    /// libiio URI, e.g. `"usb:1.6.5"` or `"ip:pluto.local"`.
    pub uri: String,
    /// RX_LO frequency in Hz.
    pub rx_freq_hz: u64,
    /// TX_LO frequency in Hz. May differ from `rx_freq_hz` for
    /// repeater duplex.
    pub tx_freq_hz: u64,
    /// AD9361 AGC mode. One of `manual`, `fast_attack`, `slow_attack`,
    /// `hybrid` (the chip's `gain_control_mode_available` set). Only
    /// when this is `manual` does [`Self::rx_gain_db`] take effect; in
    /// the three AGC modes the AD9361 picks gain dynamically.
    pub rx_gain_mode: RxGainMode,
    /// Manual RX gain in dB. Range `[-3, 71]` step 1 dB on the AD9363.
    /// Only honoured when [`Self::rx_gain_mode`] is
    /// [`RxGainMode::Manual`].
    pub rx_gain_db: i32,
    /// TX attenuation in dB (positive value = more attenuation; the
    /// AD9361 driver internally writes it as a negative `hardwaregain`).
    /// Range `[0.0, 89.75]` dB step 0.25. Lower value = more output
    /// power; 0 dB = full Pluto rated output (~7 dBm).
    pub tx_attenuation_db: f64,
    /// AD9361 RF bandwidth, in Hz. 200 kHz is comfortable for ±5 kHz
    /// deviation NBFM; the 4× FIR + decimation chain shapes the rest.
    /// Range `[200000, 56000000]` on RX and `[200000, 40000000]` on TX.
    pub rf_bandwidth_hz: u64,
    /// Whether to attempt [`crate::PREFERRED_SAMPLE_RATE_HZ`]
    /// (576 kS/s, ratio ÷12) first. Always falls back to
    /// [`crate::FALLBACK_SAMPLE_RATE_HZ`] (2304 kS/s, ratio ÷48) on
    /// `EINVAL`.
    pub prefer_low_rate: bool,
    /// Maximum RX FM deviation in Hz, used to scale the discriminator
    /// gain on the demodulator (`gain = sample_rate / (2π · max_dev)`).
    /// 5000 Hz = standard NBFM; 2500 Hz = narrow NFM (some repeaters,
    /// PMR-style channels). Picking a value below the actual on-air
    /// deviation clips the audio amplitude; picking it above attenuates
    /// it. Default 5000.
    pub rx_max_deviation_hz: f32,
    /// Effective TX FM deviation in Hz (= preset + fine-tune offset
    /// already resolved by the caller). Drives the phase modulator's
    /// `k_p` so audio at unit amplitude produces ±tx_deviation_hz on
    /// the air. Linearly scales `PhaseMod::DEFAULT_K_P` from the 5000 Hz
    /// calibration. Default 5000.
    pub tx_deviation_hz: f32,
}

impl Default for PlutoConfig {
    fn default() -> Self {
        Self {
            uri: crate::DEFAULT_URI.to_string(),
            rx_freq_hz: 145_500_000,
            tx_freq_hz: 145_500_000,
            rx_gain_mode: RxGainMode::Manual,
            rx_gain_db: 30,
            tx_attenuation_db: 10.0,
            rf_bandwidth_hz: 200_000,
            prefer_low_rate: true,
            rx_max_deviation_hz: 5000.0,
            tx_deviation_hz: 5000.0,
        }
    }
}

/// AD9361 RX gain control modes. Maps directly to the strings the
/// libiio attribute `voltage0/gain_control_mode` accepts.
///
/// Recommendations:
/// * **Manual** — best when the operator knows the expected RF level
///   (lab tests, fixed-power loopback). Uses [`PlutoConfig::rx_gain_db`].
/// * **SlowAttack** — the AD9361 driver's own default. Smooth gain
///   tracking; fine for stable HAM-band receive.
/// * **FastAttack** — reacts quickly to bursty signals. Good for
///   intermittent transmissions but can pump on noise.
/// * **Hybrid** — ADI's compromise; works well for speech / NBFM
///   audio with occasional level swings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxGainMode {
    Manual,
    FastAttack,
    SlowAttack,
    Hybrid,
}

impl RxGainMode {
    /// libiio attribute string for this mode.
    pub fn as_iio_str(&self) -> &'static str {
        match self {
            RxGainMode::Manual => "manual",
            RxGainMode::FastAttack => "fast_attack",
            RxGainMode::SlowAttack => "slow_attack",
            RxGainMode::Hybrid => "hybrid",
        }
    }

    /// Parse from the same string set the libiio attribute reports.
    /// Returns `None` for any unknown value (caller decides whether
    /// to fall back to a default or surface an error).
    pub fn from_iio_str(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(Self::Manual),
            "fast_attack" => Some(Self::FastAttack),
            "slow_attack" => Some(Self::SlowAttack),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
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

/// Live, fully-configured Pluto session.
///
/// Owns the libiio `Context` plus the three device handles. **Channels
/// are intentionally not stored** because `industrial_io::Channel`
/// holds a raw pointer and is therefore `!Send`, which would prevent
/// moving the session into a worker thread (the natural place to run
/// the libiio buffer pump). Channel handles are cheap to re-fetch via
/// the helpers on this type — `find_channel` is just a name lookup
/// against the parent device.
///
/// `Context` is `Send + Sync` (cloning bumps an internal `Arc`) and
/// `Device` is manually-marked `Send`, so cloning a session is safe
/// and used by the loopback test to drive RX and TX simultaneously
/// from one open Pluto context.
#[derive(Clone)]
pub struct PlutoSession {
    pub ctx: Context,
    pub phy: Device,
    pub rx_buffer_dev: Device,
    pub tx_buffer_dev: Device,
    pub negotiated_rate: NegotiatedRate,
    /// RX FM max deviation copied from the [`PlutoConfig`] used to
    /// open this session. Read by `rx::capture_loop` to size the
    /// `QuadratureDemod` discriminator gain. Carried on the session
    /// rather than passed as an extra capture-loop argument so the
    /// existing `start_on` / `capture_loop` signatures stay compatible
    /// with the loopback test path.
    pub rx_max_deviation_hz: f32,
    /// TX FM deviation copied from the [`PlutoConfig`]. Read by
    /// `tx::run_tx` to scale the `PhaseMod`'s `k_p` from its 5 kHz
    /// calibration. Same rationale as `rx_max_deviation_hz`.
    pub tx_deviation_hz: f32,
}

impl std::fmt::Debug for PlutoSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `industrial_io::Device` doesn't impl Debug in a useful way,
        // so summarize what the user actually wants to see.
        f.debug_struct("PlutoSession")
            .field("rate_hz", &self.negotiated_rate.sample_rate_hz)
            .field("ratio", &self.negotiated_rate.ratio)
            .finish()
    }
}

impl PlutoSession {
    /// RX baseband control channel on `ad9361-phy` (`voltage0` input).
    /// This is where `hardwaregain`, `gain_control_mode`,
    /// `rf_bandwidth`, `filter_fir_en`, `sampling_frequency` live.
    pub fn rx_baseband_chan(&self) -> Result<Channel, PlutoError> {
        self.phy
            .find_channel("voltage0", Direction::Input)
            .ok_or(PlutoError::Attribute {
                device: iio_names::PHY,
                attr: "voltage0 (input)",
                detail: "RX baseband channel not present".into(),
            })
    }

    /// TX baseband control channel on `ad9361-phy` (`voltage0` output).
    pub fn tx_baseband_chan(&self) -> Result<Channel, PlutoError> {
        self.phy
            .find_channel("voltage0", Direction::Output)
            .ok_or(PlutoError::Attribute {
                device: iio_names::PHY,
                attr: "voltage0 (output)",
                detail: "TX baseband channel not present".into(),
            })
    }

    /// RX_LO frequency channel on `ad9361-phy` (`altvoltage0` output).
    pub fn rx_lo_chan(&self) -> Result<Channel, PlutoError> {
        self.phy
            .find_channel("altvoltage0", Direction::Output)
            .ok_or(PlutoError::Attribute {
                device: iio_names::PHY,
                attr: "altvoltage0",
                detail: "RX_LO channel not present".into(),
            })
    }

    /// TX_LO frequency channel on `ad9361-phy` (`altvoltage1` output).
    pub fn tx_lo_chan(&self) -> Result<Channel, PlutoError> {
        self.phy
            .find_channel("altvoltage1", Direction::Output)
            .ok_or(PlutoError::Attribute {
                device: iio_names::PHY,
                attr: "altvoltage1",
                detail: "TX_LO channel not present".into(),
            })
    }
}

/// Open a libiio context and apply the static parts of `config`.
///
/// Sequence (matches the `pyadi-iio` / `gr-iio` recipe):
///
/// 1. Open context at `config.uri`.
/// 2. Locate `ad9361-phy`, `cf-ad9361-lpc`, `cf-ad9361-dds-core-lpc`.
/// 3. Set RX gain control mode = `manual`, RX hardwaregain, TX
///    attenuation, RX/TX RF bandwidth (all on the phy device).
/// 4. Set RX_LO and TX_LO frequencies (altvoltage0 / altvoltage1).
/// 5. Generate a 4× decimating FIR + format the .ftr blob, write to
///    the device-attr `filter_fir_config`.
/// 6. Enable per-channel `filter_fir_en = 1` on the RX/TX baseband
///    channels.
/// 7. Try `sampling_frequency = 576000` on both directions; if either
///    rejects with `EINVAL`, retry at `2304000`.
///
/// The session that comes back is "armed" — buffer devices are ready
/// for the rx / tx modules to call `create_buffer` against them.
pub fn open(config: &PlutoConfig) -> Result<PlutoSession, PlutoError> {
    let ctx = Context::from_uri(&config.uri).map_err(|e| PlutoError::OpenContext {
        uri: config.uri.clone(),
        source: e,
    })?;

    let phy = ctx
        .find_device(iio_names::PHY)
        .ok_or(PlutoError::DeviceNotFound {
            name: iio_names::PHY,
        })?;
    let rx_buffer_dev =
        ctx.find_device(iio_names::RX_BUFFER)
            .ok_or(PlutoError::DeviceNotFound {
                name: iio_names::RX_BUFFER,
            })?;
    let tx_buffer_dev =
        ctx.find_device(iio_names::TX_BUFFER)
            .ok_or(PlutoError::DeviceNotFound {
                name: iio_names::TX_BUFFER,
            })?;

    // Per-channel handles. The AD9361 driver puts gain control + FIR
    // enable + bandwidth on the per-direction baseband voltage0
    // channels rather than on the device, so we keep these around.
    let rx_chan = phy
        .find_channel("voltage0", Direction::Input)
        .ok_or(PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0 (input)",
            detail: "RX baseband channel not present".into(),
        })?;
    let tx_chan = phy
        .find_channel("voltage0", Direction::Output)
        .ok_or(PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0 (output)",
            detail: "TX baseband channel not present".into(),
        })?;
    let rx_lo = phy
        .find_channel("altvoltage0", Direction::Output)
        .ok_or(PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "altvoltage0",
            detail: "RX_LO channel not present".into(),
        })?;
    let tx_lo = phy
        .find_channel("altvoltage1", Direction::Output)
        .ok_or(PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "altvoltage1",
            detail: "TX_LO channel not present".into(),
        })?;

    // --- RX gain: program the chosen AGC mode, then in `manual` mode
    // also push the explicit gain. In any AGC mode we leave the chip
    // to do its thing — writing `hardwaregain` in non-manual modes
    // would be ignored by the driver anyway.
    rx_chan
        .attr_write_str("gain_control_mode", config.rx_gain_mode.as_iio_str())
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/gain_control_mode",
            detail: format!(
                "mode '{}' rejected: {e}",
                config.rx_gain_mode.as_iio_str()
            ),
        })?;
    if config.rx_gain_mode == RxGainMode::Manual {
        rx_chan
            .attr_write_int("hardwaregain", config.rx_gain_db as i64)
            .map_err(|e| PlutoError::Attribute {
                device: iio_names::PHY,
                attr: "voltage0/hardwaregain (RX)",
                detail: format!("rx_gain_db = {} rejected: {e}", config.rx_gain_db),
            })?;
    }

    // --- TX attenuation: AD9361 driver expects a negative value in dB.
    let tx_hw_gain = -config.tx_attenuation_db.abs();
    tx_chan
        .attr_write_float("hardwaregain", tx_hw_gain)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/hardwaregain (TX)",
            detail: format!("tx_attenuation_db = {} rejected: {e}", config.tx_attenuation_db),
        })?;

    // --- RF bandwidths.
    rx_chan
        .attr_write_int("rf_bandwidth", config.rf_bandwidth_hz as i64)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/rf_bandwidth (RX)",
            detail: format!("{} rejected: {e}", config.rf_bandwidth_hz),
        })?;
    tx_chan
        .attr_write_int("rf_bandwidth", config.rf_bandwidth_hz as i64)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/rf_bandwidth (TX)",
            detail: format!("{} rejected: {e}", config.rf_bandwidth_hz),
        })?;

    // --- LO frequencies.
    rx_lo
        .attr_write_int("frequency", config.rx_freq_hz as i64)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "altvoltage0/frequency (RX_LO)",
            detail: format!("{} Hz rejected: {e}", config.rx_freq_hz),
        })?;
    tx_lo
        .attr_write_int("frequency", config.tx_freq_hz as i64)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "altvoltage1/frequency (TX_LO)",
            detail: format!("{} Hz rejected: {e}", config.tx_freq_hz),
        })?;

    // --- FIR: load taps, enable per-direction, then negotiate rate.
    load_decim4_fir(&phy, &rx_chan, &tx_chan)?;
    let negotiated_rate = negotiate_sample_rate(&rx_chan, &tx_chan, config.prefer_low_rate)?;

    Ok(PlutoSession {
        ctx,
        phy,
        rx_buffer_dev,
        tx_buffer_dev,
        negotiated_rate,
        rx_max_deviation_hz: config.rx_max_deviation_hz,
        tx_deviation_hz: config.tx_deviation_hz,
    })
}

/// Generate the 4× decimating FIR + load it onto the AD9361.
///
/// On EINVAL (which the AD9361 driver returns when the chosen tap
/// count or gain combo doesn't match the BBPLL state), we surface the
/// error rather than silently fall back — the caller can choose to
/// rebuild with a different rate.
fn load_decim4_fir(phy: &Device, rx_chan: &Channel, tx_chan: &Channel) -> Result<(), PlutoError> {
    let taps = design_decim4_lpf();
    let blob = format_fir_blob(&taps, &taps);

    phy.attr_write_str("filter_fir_config", &blob)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "filter_fir_config",
            detail: format!("FIR config rejected: {e}"),
        })?;
    rx_chan
        .attr_write_bool("filter_fir_en", true)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/filter_fir_en (RX)",
            detail: e.to_string(),
        })?;
    tx_chan
        .attr_write_bool("filter_fir_en", true)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/filter_fir_en (TX)",
            detail: e.to_string(),
        })?;
    Ok(())
}

/// Try [`crate::PREFERRED_SAMPLE_RATE_HZ`] first; on `EINVAL` (or any
/// other write failure) retry [`crate::FALLBACK_SAMPLE_RATE_HZ`]. The
/// rate is set on both RX and TX baseband channels, which on the
/// AD9361 are clocked together — writing one effectively writes both,
/// but we set both explicitly for robustness against driver versions.
fn negotiate_sample_rate(
    rx_chan: &Channel,
    tx_chan: &Channel,
    prefer_low_rate: bool,
) -> Result<NegotiatedRate, PlutoError> {
    let candidates: &[NegotiatedRate] = if prefer_low_rate {
        &[NegotiatedRate::PREFERRED, NegotiatedRate::FALLBACK]
    } else {
        &[NegotiatedRate::FALLBACK]
    };

    let mut last_err: Option<String> = None;
    for cand in candidates {
        let rx_res = rx_chan.attr_write_int("sampling_frequency", cand.sample_rate_hz as i64);
        let tx_res = tx_chan.attr_write_int("sampling_frequency", cand.sample_rate_hz as i64);
        match (rx_res, tx_res) {
            (Ok(()), Ok(())) => return Ok(*cand),
            (Err(e), _) | (_, Err(e)) => {
                last_err = Some(format!("{cand:?}: {e}"));
            }
        }
    }
    Err(PlutoError::SampleRate {
        rate: candidates
            .last()
            .map(|c| c.sample_rate_hz)
            .unwrap_or_default(),
        detail: last_err.unwrap_or_else(|| "no candidates accepted".into()),
    })
}

// ---------------------------------------------------------------------
// FIR design (pure-Rust, runtime-computed)
// ---------------------------------------------------------------------

/// Number of taps in the AD9361 RFIR/TFIR when running 4× dec/int.
/// 128 is the AD9361 hard maximum; the driver allows 16-tap multiples.
const FIR_TAP_COUNT: usize = 128;

/// Quantization peak for AD9361 FIR taps. Signed int16 range, but
/// staying a safety margin under the absolute peak prevents overflow
/// in the AD9361's accumulator on rare in-band peaks.
const FIR_QUANT_PEAK: f64 = 32_000.0;

/// Design a 128-tap low-pass FIR for AD9361 4× decimation/interpolation.
///
/// The FIR runs at 4× the requested baseband rate (so 2.304 MS/s when
/// the negotiated output rate is 576 kSa/s). For an anti-alias /
/// anti-image LPF in front of a 4× decimator/interpolator, the
/// passband edge sits at `f_out/2` and the stopband starts at the
/// folding frequency on the other side of the output Nyquist —
/// expressed in normalized FIR-rate frequency, that's
/// `f_pass = 1/8 (= 0.5 / 4)` and `f_stop ≈ 1/4`. The design here is
/// intentionally relaxed (passband to ~0.10 of the FIR rate ≈ 230 kHz
/// when running at 2.304 MS/s) — the modem only uses ~16 kHz of
/// bandwidth for ±5 kHz NBFM (Carson rule), so this leaves more than
/// 14× margin and frees the AD9361's HB chain to do most of the
/// shaping.
///
/// Window is Hamming for a clean ~52 dB stopband; coefficients are
/// quantized to int16 with a peak just under the 16-bit ceiling so
/// the AD9361's internal accumulator has headroom.
pub fn design_decim4_lpf() -> [i16; FIR_TAP_COUNT] {
    // Normalized cutoff (-6 dB point) in FIR-rate units. 0.10 means the
    // passband edge sits at 10 % of the FIR-rate, which at the
    // worst-case rate of 2.304 MS/s is 230 kHz — comfortably above any
    // realistic NBFM channel even at the FIR's input.
    const FC_NORM: f64 = 0.10;

    let n = FIR_TAP_COUNT as i32;
    let mid = (n - 1) as f64 / 2.0;
    let two_pi_fc = 2.0 * std::f64::consts::PI * FC_NORM;

    // Build float taps: windowed sinc.
    let mut taps_f = [0.0f64; FIR_TAP_COUNT];
    for (i, t) in taps_f.iter_mut().enumerate() {
        let k = i as f64 - mid;
        let sinc = if k == 0.0 {
            two_pi_fc
        } else {
            (two_pi_fc * k).sin() / k
        };
        let hamming =
            0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
        *t = sinc * hamming;
    }

    // Normalize so the absolute peak hits FIR_QUANT_PEAK.
    let peak = taps_f.iter().map(|t| t.abs()).fold(0.0f64, f64::max);
    let scale = if peak > 0.0 {
        FIR_QUANT_PEAK / peak
    } else {
        1.0
    };

    let mut taps_i16 = [0i16; FIR_TAP_COUNT];
    for (out, t) in taps_i16.iter_mut().zip(taps_f.iter()) {
        let q = (t * scale).round();
        *out = q.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
    }
    taps_i16
}

/// Format a tap pair list as the AD9361 driver's `filter_fir_config`
/// blob.
///
/// Header convention (`pyadi-iio` / `iio-fir-filter-design` idiom):
///
/// ```text
/// RX 3 GAIN -6 DEC 4
/// TX 3 GAIN 0 INT 4
/// <rx_tap0>,<tx_tap0>
/// <rx_tap1>,<tx_tap1>
/// ...
/// ```
///
/// `rx_taps` and `tx_taps` must have the same length, which must be a
/// multiple of 16 (AD9361 driver constraint).
pub fn format_fir_blob(rx_taps: &[i16], tx_taps: &[i16]) -> String {
    assert_eq!(rx_taps.len(), tx_taps.len());
    assert_eq!(rx_taps.len() % 16, 0);
    let mut blob = String::with_capacity(48 + rx_taps.len() * 14);
    blob.push_str("RX 3 GAIN -6 DEC 4\n");
    blob.push_str("TX 3 GAIN 0 INT 4\n");
    for (rx, tx) in rx_taps.iter().zip(tx_taps.iter()) {
        // The AD9361 driver tolerates either CRLF or LF terminators
        // and either tab or comma separator; comma+LF is the form the
        // ADI tooling emits.
        use std::fmt::Write;
        let _ = writeln!(&mut blob, "{rx},{tx}");
    }
    blob
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FIR is the gating piece for sub-2.083 MS/s rates. Sanity-check
    /// the design's symmetry, peak, and DC gain so a regression in the
    /// math at least shows up locally before we even talk to a Pluto.
    #[test]
    fn fir_is_symmetric_and_normalized() {
        let taps = design_decim4_lpf();
        // Linear-phase symmetry: tap[i] == tap[N-1-i].
        for i in 0..taps.len() / 2 {
            assert_eq!(
                taps[i],
                taps[taps.len() - 1 - i],
                "tap {i} not symmetric"
            );
        }
        // Quantization peak hits the documented ceiling within rounding.
        let peak = taps.iter().map(|t| t.unsigned_abs()).max().unwrap();
        assert!(
            peak as f64 >= FIR_QUANT_PEAK - 1.0 && peak as f64 <= FIR_QUANT_PEAK + 1.0,
            "peak {peak} not at FIR_QUANT_PEAK ({})",
            FIR_QUANT_PEAK
        );
        // DC gain (tap sum) is positive and within a sane band — for a
        // Hamming-windowed sinc with fc = 0.10 normalized, the float
        // sum is ~0.20 of the peak, so the int16 sum should be near
        // 0.20 × 32000 × 2π × fc ≈ a comfortably positive integer.
        let sum: i64 = taps.iter().map(|&t| t as i64).sum();
        assert!(sum > 0, "DC gain non-positive: {sum}");
    }

    #[test]
    fn fir_blob_formats_with_correct_header_and_line_count() {
        let taps = design_decim4_lpf();
        let blob = format_fir_blob(&taps, &taps);
        let mut lines = blob.lines();
        assert_eq!(lines.next(), Some("RX 3 GAIN -6 DEC 4"));
        assert_eq!(lines.next(), Some("TX 3 GAIN 0 INT 4"));
        let tap_lines: Vec<&str> = lines.collect();
        assert_eq!(tap_lines.len(), FIR_TAP_COUNT);
        // Each tap line is `<rx>,<tx>` — both values parse as i16.
        for l in &tap_lines {
            let mut it = l.split(',');
            let rx: i16 = it.next().unwrap().parse().expect("rx tap");
            let tx: i16 = it.next().unwrap().parse().expect("tx tap");
            assert!(it.next().is_none(), "extra fields on {l}");
            assert_eq!(rx, tx, "asymmetric RX/TX taps in blob");
        }
    }

    #[test]
    fn negotiated_rate_constants_match_crate_constants() {
        assert_eq!(
            NegotiatedRate::PREFERRED.sample_rate_hz,
            crate::PREFERRED_SAMPLE_RATE_HZ
        );
        assert_eq!(NegotiatedRate::PREFERRED.ratio, crate::PREFERRED_RATIO);
        assert_eq!(
            NegotiatedRate::FALLBACK.sample_rate_hz,
            crate::FALLBACK_SAMPLE_RATE_HZ
        );
        assert_eq!(NegotiatedRate::FALLBACK.ratio, crate::FALLBACK_RATIO);
        assert_eq!(
            NegotiatedRate::PREFERRED.sample_rate_hz as usize,
            modem_sdr_dsp::AUDIO_RATE as usize * NegotiatedRate::PREFERRED.ratio,
            "preferred rate must be an integer multiple of the audio rate"
        );
        assert_eq!(
            NegotiatedRate::FALLBACK.sample_rate_hz as usize,
            modem_sdr_dsp::AUDIO_RATE as usize * NegotiatedRate::FALLBACK.ratio,
            "fallback rate must be an integer multiple of the audio rate"
        );
    }
}
