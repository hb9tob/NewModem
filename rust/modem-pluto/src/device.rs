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
//! that floor by 4, so the chip then accepts 528 kSa/s
//! ([`crate::PREFERRED_SAMPLE_RATE_HZ`]).
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
/// / GUI override `center_freq_hz` and `rx_gain_db` per session.
#[derive(Clone, Debug)]
pub struct PlutoConfig {
    /// libiio URI, e.g. `"usb:1.6.5"` or `"ip:pluto.local"`.
    pub uri: String,
    /// Tuner LO. Same value used for RX and TX.
    pub center_freq_hz: u64,
    /// Manual RX gain in dB. Range `[-3, 71]` step 1 dB on the AD9363.
    pub rx_gain_db: i32,
    /// TX attenuation in dB (positive value = more attenuation; the
    /// AD9361 driver internally writes it as a negative `hardwaregain`).
    /// Range `[0.0, 89.75]` dB step 0.25.
    pub tx_attenuation_db: f64,
    /// AD9361 RF bandwidth, in Hz. 200 kHz is comfortable for ±5 kHz
    /// deviation NBFM; the 4× FIR + decimation chain shapes the rest.
    /// Range `[200000, 56000000]` on RX and `[200000, 40000000]` on TX.
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

/// Live, fully-configured Pluto session.
///
/// Owns the libiio `Context` plus the three device handles and the
/// per-direction baseband channel handles (the AD9361 control points
/// for hardware gain, RF bandwidth, FIR enable). RX / TX modules
/// borrow buffer devices from here; the phy device handle stays put
/// for runtime gain / freq retuning.
pub struct PlutoSession {
    pub ctx: Context,
    pub phy: Device,
    pub rx_buffer_dev: Device,
    pub tx_buffer_dev: Device,
    pub rx_chan: Channel,
    pub tx_chan: Channel,
    pub negotiated_rate: NegotiatedRate,
}

impl std::fmt::Debug for PlutoSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `industrial_io::Device` / `Channel` don't impl Debug usefully,
        // so summarize what the user actually wants to see.
        f.debug_struct("PlutoSession")
            .field("rate_hz", &self.negotiated_rate.sample_rate_hz)
            .field("ratio", &self.negotiated_rate.ratio)
            .finish()
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
/// 7. Try `sampling_frequency = 528000` on both directions; if either
///    rejects with `EINVAL`, retry at `960000`.
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

    // --- RX gain: switch to manual mode then write the requested gain.
    rx_chan
        .attr_write_str("gain_control_mode", "manual")
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/gain_control_mode",
            detail: format!("manual mode rejected: {e}"),
        })?;
    rx_chan
        .attr_write_int("hardwaregain", config.rx_gain_db as i64)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "voltage0/hardwaregain (RX)",
            detail: format!("rx_gain_db = {} rejected: {e}", config.rx_gain_db),
        })?;

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
        .attr_write_int("frequency", config.center_freq_hz as i64)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "altvoltage0/frequency (RX_LO)",
            detail: format!("{} Hz rejected: {e}", config.center_freq_hz),
        })?;
    tx_lo
        .attr_write_int("frequency", config.center_freq_hz as i64)
        .map_err(|e| PlutoError::Attribute {
            device: iio_names::PHY,
            attr: "altvoltage1/frequency (TX_LO)",
            detail: format!("{} Hz rejected: {e}", config.center_freq_hz),
        })?;

    // --- FIR: load taps, enable per-direction, then negotiate rate.
    load_decim4_fir(&phy, &rx_chan, &tx_chan)?;
    let negotiated_rate = negotiate_sample_rate(&rx_chan, &tx_chan, config.prefer_528k)?;

    Ok(PlutoSession {
        ctx,
        phy,
        rx_buffer_dev,
        tx_buffer_dev,
        rx_chan,
        tx_chan,
        negotiated_rate,
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
    prefer_528k: bool,
) -> Result<NegotiatedRate, PlutoError> {
    let candidates: &[NegotiatedRate] = if prefer_528k {
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
/// The FIR runs at 4× the requested baseband rate (so 2.112 MS/s when
/// the negotiated output rate is 528 kSa/s). For an anti-alias /
/// anti-image LPF in front of a 4× decimator/interpolator, the
/// passband edge sits at `f_out/2` and the stopband starts at the
/// folding frequency on the other side of the output Nyquist —
/// expressed in normalized FIR-rate frequency, that's
/// `f_pass = 1/8 (= 0.5 / 4)` and `f_stop ≈ 1/4`. The design here is
/// intentionally relaxed (passband to ~0.10 of the FIR rate ≈ 211 kHz
/// when running at 2.112 MS/s) — the modem only uses ~16 kHz of
/// bandwidth for ±5 kHz NBFM (Carson rule), so this leaves more than
/// 13× margin and frees the AD9361's HB chain to do most of the
/// shaping.
///
/// Window is Hamming for a clean ~52 dB stopband; coefficients are
/// quantized to int16 with a peak just under the 16-bit ceiling so
/// the AD9361's internal accumulator has headroom.
pub fn design_decim4_lpf() -> [i16; FIR_TAP_COUNT] {
    // Normalized cutoff (-6 dB point) in FIR-rate units. 0.10 means the
    // passband edge sits at 10 % of the FIR-rate, which at the
    // worst-case rate of 2.112 MS/s is 211 kHz — comfortably above any
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
