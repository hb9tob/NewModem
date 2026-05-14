//! Channel-sounder analyser — turns recorded probe waveforms into
//! quantitative channel parameters.
//!
//! Each function takes a recorded f32 slice at a known sample rate plus
//! the probe descriptors (which frequencies, which level stamps, ...)
//! and returns a typed measurement struct. The structs are
//! `serde::Serialize`-able so they slot directly into the
//! `ChannelSignature` JSON written by
//! `modem-worker-base/src/sounder.rs`.
//!
//! All public helpers use single-bin Goertzel DFTs where possible
//! (O(N) per frequency, no plan allocation, lower noise than a full
//! FFT for tone extraction). The Hilbert transform is FFT-based via
//! `rustfft` (already cached at the module level for repeat calls).

use std::f64::consts::PI;
use std::sync::Arc;

use num_complex::Complex64;
use rustfft::{Fft, FftPlanner};

use crate::probe::LevelStamp;

// --- Output structs -------------------------------------------------------

/// Result of a single-tone measurement.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToneMeasure {
    /// Peak amplitude of the tone (linear, like the input).
    pub amplitude: f32,
    /// Tone phase at sample 0 in radians, wrapped to (-π, π].
    pub phase_rad: f32,
    /// SNR estimate in dB: 10·log10(P_signal / P_residual), where
    /// P_residual is obtained by subtracting the recovered tone from
    /// the recording.
    pub snr_db: f32,
}

/// Result of a two-tone IMD3 measurement.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TwoToneMeasure {
    pub a1_dbfs: f32,
    pub a2_dbfs: f32,
    /// Amplitude of the lower IMD3 (`2·f1 - f2`) in dBc relative to a1.
    pub imd3_low_dbc: f32,
    /// Amplitude of the upper IMD3 (`2·f2 - f1`) in dBc relative to a2.
    pub imd3_high_dbc: f32,
    /// Output-referred third-order intercept in dBFS, computed as
    /// `(a1_dbfs + a2_dbfs) / 2 - 0.5 * mean(imd3_low_dbc, imd3_high_dbc)`.
    /// Higher = more linear chain.
    pub ip3_dbfs: f32,
}

/// Result of a chirp measurement: instantaneous group delay vs frequency.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChirpMeasure {
    /// One sample per chunk: (freq_hz, group_delay_us). The freqs cover
    /// the chirp range linearly; the delays are measured relative to the
    /// median (so a flat channel gives ≈ 0 µs deviation across the band).
    pub group_delay_per_freq: Vec<(f64, f32)>,
    /// Lower / upper edges of the -3 dB passband, in Hz. The chirp's
    /// instantaneous-amplitude envelope is FFT-windowed and the -3 dB
    /// extents are extracted; useful to detect the audio LPF and the
    /// CTCSS HPF.
    pub bw_3db_hz: (f32, f32),
}

/// Result of a multi-tone frequency-response sweep.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MultiToneMeasure {
    /// Per-tone (freq_hz, gain_db). Gain is referenced to the strongest
    /// bin so the curve is normalised to 0 dB at its peak — what
    /// matters for channel characterisation is the *shape*, not the
    /// absolute level.
    pub gain_db_per_freq: Vec<(f64, f32)>,
    /// Noise floor estimate in dBFS, measured at a few "gap" frequencies
    /// midway between the tones.
    pub noise_floor_dbfs: f32,
}

/// Result of an amplitude / level sweep.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LevelSweepMeasure {
    /// (input_dbfs, output_dbfs) per level segment, in ascending input
    /// order (lowest level first → highest last).
    pub am_am_curve: Vec<(f32, f32)>,
    /// (input_dbfs, phase_rad) — AM-PM. Phase referenced to the
    /// lowest-level segment.
    pub am_pm_curve: Vec<(f32, f32)>,
    /// 1 dB compression point in input dBFS — the level where the
    /// AM-AM curve has fallen 1 dB below the small-signal linear
    /// extrapolation. `f32::NAN` if the sweep doesn't reach compression.
    pub p1db_dbfs: f32,
    /// Recommended TX-level sweet spot in input dBFS: the level that
    /// maximises measured signal-to-distortion ratio along the sweep.
    /// Heuristic: P1dB − 3 dB, clamped to the observed range.
    pub sweet_spot_dbfs: f32,
    /// SNR per level segment (dB) — useful to plot the noise vs
    /// distortion trade-off directly.
    pub snr_db_per_level: Vec<(f32, f32)>,
}

// --- Goertzel single-bin DFT ---------------------------------------------

/// Single-bin DFT via Goertzel's algorithm. Returns `(amplitude, phase)`
/// where amplitude is normalised so that a pure sine wave at `f_hz`
/// of input peak `A` produces output `≈ A`. Phase is the sine-phase at
/// sample 0, in (-π, π].
pub fn goertzel(audio: &[f32], sr: u32, f_hz: f64) -> (f32, f32) {
    let omega = 2.0 * PI * f_hz / sr as f64;
    let cos_om = omega.cos();
    let coeff = 2.0 * cos_om;
    let mut s_prev = 0.0_f64;
    let mut s_prev2 = 0.0_f64;
    for &x in audio {
        let s = (x as f64) + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let re = s_prev - s_prev2 * cos_om;
    let im = s_prev2 * omega.sin();
    let mag = (re * re + im * im).sqrt() * 2.0 / audio.len() as f64;
    let phase = im.atan2(re);
    (mag as f32, phase as f32)
}

// --- Sync-marker cross-correlation ---------------------------------------

/// Find the sample index in `audio` where `template` aligns best
/// (FFT-based cross-correlation, peak picking).
///
/// `audio` is the receiver-side capture (possibly minutes long; will
/// contain band noise before the TX wake-up tone, the wake-up tone
/// itself, the sync marker, and the probes). `template` is the
/// transmit-side chirp returned by [`crate::probe::sync_marker`].
///
/// Returns the sample index of the start of the matched template if
/// the correlation peak exceeds `peak_threshold_factor` times the
/// background RMS of the correlation (typical: 6.0 → false-positive
/// rate roughly 10⁻⁹ on AWGN). `None` if no peak meets the
/// threshold — caller should treat this as "lost sync".
///
/// FFT-based: O(N log N) on the audio length. A 60 s capture at
/// 48 kHz fits in ≈ 4 M-pt FFT, ≈ 80 ms on Pi5.
pub fn find_sync_marker(
    audio: &[f32],
    template: &[f32],
    peak_threshold_factor: f32,
) -> Option<usize> {
    let n_a = audio.len();
    let n_t = template.len();
    if n_t == 0 || n_a < n_t {
        return None;
    }
    // Pad both to a common length ≥ n_a + n_t (linear, not circular,
    // correlation). Next power of 2 keeps rustfft fast even on
    // not-radix-2 inputs.
    let fft_len = (n_a + n_t).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fwd: Arc<dyn Fft<f64>> = planner.plan_fft_forward(fft_len);
    let inv: Arc<dyn Fft<f64>> = planner.plan_fft_inverse(fft_len);
    // Pad-and-FFT audio.
    let mut buf_a: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); fft_len];
    for (i, &x) in audio.iter().enumerate() {
        buf_a[i] = Complex64::new(x as f64, 0.0);
    }
    fwd.process(&mut buf_a);
    // Pad-and-FFT template, then conjugate so multiply == correlate.
    let mut buf_t: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); fft_len];
    for (i, &x) in template.iter().enumerate() {
        buf_t[i] = Complex64::new(x as f64, 0.0);
    }
    fwd.process(&mut buf_t);
    for b in buf_t.iter_mut() {
        *b = b.conj();
    }
    // Pointwise multiply, IFFT.
    for i in 0..fft_len {
        buf_a[i] *= buf_t[i];
    }
    inv.process(&mut buf_a);
    // rustfft inverse is not normalised; we only care about peak
    // position so the scale factor is irrelevant.
    let valid_end = n_a - n_t + 1; // last possible start index
    let mut best_idx = 0usize;
    let mut best = f64::NEG_INFINITY;
    let mut sum_sq = 0.0_f64;
    for i in 0..valid_end {
        let v = buf_a[i].re;
        sum_sq += v * v;
        if v > best {
            best = v;
            best_idx = i;
        }
    }
    let rms = (sum_sq / valid_end as f64).sqrt();
    if best > peak_threshold_factor as f64 * rms {
        Some(best_idx)
    } else {
        None
    }
}

// --- Hilbert (FFT-based analytic signal) ---------------------------------

/// FFT-based Hilbert transform: returns the analytic signal
/// `re + j·H{re}` such that the negative-frequency bins are zeroed and
/// the positive ones doubled (DC and Nyquist preserved). Works on any
/// length (rustfft handles non-power-of-2).
///
/// The result has the same length as the input. Used by [`measure_chirp`]
/// to extract the instantaneous frequency.
pub fn hilbert_fft(re: &[f32]) -> Vec<Complex64> {
    let n = re.len();
    if n == 0 {
        return Vec::new();
    }
    let mut planner = FftPlanner::<f64>::new();
    let forward: Arc<dyn Fft<f64>> = planner.plan_fft_forward(n);
    let inverse: Arc<dyn Fft<f64>> = planner.plan_fft_inverse(n);
    let mut buf: Vec<Complex64> =
        re.iter().map(|&x| Complex64::new(x as f64, 0.0)).collect();
    forward.process(&mut buf);
    // Hilbert mask: keep DC + Nyquist (if exists), double positives, zero negatives.
    let half = if n % 2 == 0 { n / 2 } else { (n + 1) / 2 };
    for (i, b) in buf.iter_mut().enumerate() {
        if i == 0 {
            // DC unchanged
        } else if i < half {
            *b *= 2.0;
        } else if i == half && n % 2 == 0 {
            // Nyquist bin, unchanged
        } else {
            *b = Complex64::new(0.0, 0.0);
        }
    }
    inverse.process(&mut buf);
    // rustfft does not normalise the inverse — divide by N.
    let scale = 1.0 / n as f64;
    for b in buf.iter_mut() {
        *b *= scale;
    }
    buf
}

// --- Tone measurement ----------------------------------------------------

/// Extract amplitude + phase of a known-frequency tone, and estimate
/// the SNR by subtracting the recovered tone from the recording.
///
/// Internally uses the exact 2-parameter least-squares fit of
/// `{cos(ωn), sin(ωn)}` to the audio — this is what makes the
/// reconstruction subtractable cleanly (and therefore the SNR
/// estimate honest), unlike the bare Goertzel magnitude+phase pair
/// where the phase convention is annoying to round-trip.
pub fn measure_tone(audio: &[f32], sr: u32, f_hz: f64) -> ToneMeasure {
    let n = audio.len();
    if n == 0 {
        return ToneMeasure {
            amplitude: 0.0,
            phase_rad: 0.0,
            snr_db: f32::NEG_INFINITY,
        };
    }
    let omega = 2.0 * PI * f_hz / sr as f64;
    let mut sum_cos = 0.0_f64;
    let mut sum_sin = 0.0_f64;
    for (i, &x) in audio.iter().enumerate() {
        let p = omega * i as f64;
        sum_cos += x as f64 * p.cos();
        sum_sin += x as f64 * p.sin();
    }
    let a_cos = 2.0 * sum_cos / n as f64;
    let a_sin = 2.0 * sum_sin / n as f64;
    let amp = (a_cos * a_cos + a_sin * a_sin).sqrt();
    let phase_rad = a_sin.atan2(a_cos);
    // SNR via exact LS residual: x_fit[n] = a_cos·cos(ωn) + a_sin·sin(ωn).
    let mut residual_pwr = 0.0_f64;
    for (i, &x) in audio.iter().enumerate() {
        let p = omega * i as f64;
        let recon = a_cos * p.cos() + a_sin * p.sin();
        let e = x as f64 - recon;
        residual_pwr += e * e;
    }
    residual_pwr /= n as f64;
    let signal_pwr = 0.5 * amp * amp;
    let snr = if residual_pwr > 1e-20 {
        10.0 * (signal_pwr / residual_pwr).log10()
    } else {
        f64::INFINITY
    };
    ToneMeasure {
        amplitude: amp as f32,
        phase_rad: phase_rad as f32,
        snr_db: snr as f32,
    }
}

// --- Two-tone IMD3 -------------------------------------------------------

/// Two-tone IMD3 measurement. Extracts amplitudes at f1, f2 (fundamentals)
/// and at 2·f1-f2, 2·f2-f1 (third-order intermods). Reports dBc relative
/// to the corresponding fundamental + output-referred IP3 estimate.
pub fn measure_two_tone(
    audio: &[f32],
    sr: u32,
    f1: f64,
    f2: f64,
) -> TwoToneMeasure {
    let (a1, _) = goertzel(audio, sr, f1);
    let (a2, _) = goertzel(audio, sr, f2);
    let (imd_lo, _) = goertzel(audio, sr, 2.0 * f1 - f2);
    let (imd_hi, _) = goertzel(audio, sr, 2.0 * f2 - f1);
    let a1_db = 20.0 * (a1.max(1e-20) as f64).log10();
    let a2_db = 20.0 * (a2.max(1e-20) as f64).log10();
    let imd_lo_db = 20.0 * (imd_lo.max(1e-20) as f64).log10();
    let imd_hi_db = 20.0 * (imd_hi.max(1e-20) as f64).log10();
    let imd_lo_dbc = (imd_lo_db - a1_db) as f32;
    let imd_hi_dbc = (imd_hi_db - a2_db) as f32;
    // IP3_out [dBFS] = P_fund + |IMD3_dBc| / 2.  Average over both sides.
    let mean_fund = 0.5 * (a1_db + a2_db);
    let mean_imd3_dbc = 0.5 * (imd_lo_dbc + imd_hi_dbc) as f64;
    let ip3 = mean_fund + 0.5 * (-mean_imd3_dbc);
    TwoToneMeasure {
        a1_dbfs: a1_db as f32,
        a2_dbfs: a2_db as f32,
        imd3_low_dbc: imd_lo_dbc,
        imd3_high_dbc: imd_hi_dbc,
        ip3_dbfs: ip3 as f32,
    }
}

// --- Chirp ----------------------------------------------------------------

/// Measure group delay vs frequency from a linear chirp. The analyser
/// Hilbert-transforms the recording, unwraps the phase, takes the
/// derivative to recover the instantaneous frequency, and compares it
/// to the expected linear ramp. Deviations from the expected timing
/// give the relative group delay across the band.
///
/// Group delay is reported as a delta from the median (centred chart) —
/// useful for spotting bumps near the audio LPF / sub-audio HPF edges
/// without committing to an absolute reference.
pub fn measure_chirp(
    audio: &[f32],
    sr: u32,
    f0: f64,
    f1: f64,
) -> ChirpMeasure {
    let n = audio.len();
    if n < 64 {
        return ChirpMeasure {
            group_delay_per_freq: Vec::new(),
            bw_3db_hz: (0.0, 0.0),
        };
    }
    let dur_s = n as f64 / sr as f64;
    let slope = (f1 - f0) / dur_s;
    let z = hilbert_fft(audio);
    // Instantaneous phase, then unwrap.
    let mut phase = Vec::with_capacity(n);
    for c in &z {
        phase.push(c.im.atan2(c.re));
    }
    // Unwrap.
    let mut acc = 0.0;
    let mut last = phase[0];
    let mut unwrapped = Vec::with_capacity(n);
    unwrapped.push(phase[0]);
    for &p in &phase[1..] {
        let mut dp = p - last;
        while dp > PI {
            dp -= 2.0 * PI;
        }
        while dp < -PI {
            dp += 2.0 * PI;
        }
        acc += dp;
        unwrapped.push(phase[0] + acc);
        last = p;
    }
    // Instantaneous frequency = (1/2π) d phase / dt
    // Group delay at freq f: position where IF matches f, deviation in
    // µs from expected position (= (f - f0) / slope).
    // We sample at a coarse grid of freq bins (32 by default). The
    // raw per-sample IF derivative is too noisy from numerical
    // Hilbert edges + arctan precision, so we smooth it with a
    // 200-sample (≈4 ms at 48 kHz) moving average — broad enough to
    // kill the wobble, narrow enough that genuine group-delay bumps
    // in the audio passband remain visible.
    let n_bins = 32_usize.min(n / 64);
    let raw_if: Vec<f64> = (0..n - 1)
        .map(|i| (unwrapped[i + 1] - unwrapped[i]) * sr as f64 / (2.0 * PI))
        .collect();
    let smooth_w = 200_usize.min(raw_if.len() / 4).max(1);
    let mut inst_freq: Vec<f64> = Vec::with_capacity(raw_if.len());
    let mut running = 0.0_f64;
    let mut window: std::collections::VecDeque<f64> =
        std::collections::VecDeque::with_capacity(smooth_w);
    for &v in &raw_if {
        if window.len() == smooth_w {
            running -= window.pop_front().unwrap();
        }
        running += v;
        window.push_back(v);
        inst_freq.push(running / window.len() as f64);
    }
    let mut gd: Vec<(f64, f32)> = Vec::with_capacity(n_bins);
    for k in 0..n_bins {
        let f_target = f0 + (k as f64 + 0.5) / n_bins as f64 * (f1 - f0);
        let expected_idx = ((f_target - f0) / slope * sr as f64) as i64;
        // Find the local sample where inst_freq best matches f_target.
        // Search ±1 % of n around expected_idx.
        let win = (n as i64 / 100).max(8);
        let lo = (expected_idx - win).max(0) as usize;
        let hi = ((expected_idx + win) as usize).min(inst_freq.len() - 1);
        let mut best_idx = expected_idx as usize;
        let mut best_err = f64::INFINITY;
        for j in lo..=hi {
            let err = (inst_freq[j] - f_target).abs();
            if err < best_err {
                best_err = err;
                best_idx = j;
            }
        }
        let delta_samples = best_idx as i64 - expected_idx;
        let delta_us = (delta_samples as f64 / sr as f64) * 1e6;
        gd.push((f_target, delta_us as f32));
    }
    // Centre on median.
    let mut vals: Vec<f32> = gd.iter().map(|&(_, v)| v).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = vals[vals.len() / 2];
    for (_, v) in gd.iter_mut() {
        *v -= median;
    }
    // -3 dB BW: envelope = |z|, smooth, find edges where envelope <
    // peak/√2.
    let env: Vec<f64> = z.iter().map(|c| (c.re * c.re + c.im * c.im).sqrt()).collect();
    let env_peak = env.iter().cloned().fold(0.0_f64, f64::max);
    let thresh = env_peak / std::f64::consts::SQRT_2;
    let mut low_edge_idx = 0usize;
    while low_edge_idx < n && env[low_edge_idx] < thresh {
        low_edge_idx += 1;
    }
    let mut high_edge_idx = n.saturating_sub(1);
    while high_edge_idx > 0 && env[high_edge_idx] < thresh {
        high_edge_idx -= 1;
    }
    let low_t = low_edge_idx as f64 / sr as f64;
    let high_t = high_edge_idx as f64 / sr as f64;
    let low_f = f0 + slope * low_t;
    let high_f = f0 + slope * high_t;
    ChirpMeasure {
        group_delay_per_freq: gd,
        bw_3db_hz: (low_f as f32, high_f as f32),
    }
}

// --- Multitone -----------------------------------------------------------

/// Per-frequency gain (normalised to the strongest bin) + noise floor.
pub fn measure_multitone(
    audio: &[f32],
    sr: u32,
    freqs_hz: &[f64],
) -> MultiToneMeasure {
    let mut bins: Vec<(f64, f32)> = freqs_hz
        .iter()
        .map(|&f| {
            let (amp, _) = goertzel(audio, sr, f);
            let db = 20.0 * (amp.max(1e-20) as f64).log10();
            (f, db as f32)
        })
        .collect();
    let peak_db = bins.iter().map(|&(_, d)| d).fold(f32::MIN, f32::max);
    for (_, d) in bins.iter_mut() {
        *d -= peak_db;
    }
    // Noise floor: probe a handful of "gap" frequencies that fall midway
    // between adjacent tones — those should be pure noise.
    let mut sorted = freqs_hz.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut gap_db_sum = 0.0_f64;
    let mut gap_count = 0usize;
    for w in sorted.windows(2) {
        let f_mid = 0.5 * (w[0] + w[1]);
        let (amp, _) = goertzel(audio, sr, f_mid);
        gap_db_sum += 20.0 * (amp.max(1e-20) as f64).log10();
        gap_count += 1;
    }
    let noise_floor = if gap_count > 0 {
        (gap_db_sum / gap_count as f64) as f32
    } else {
        f32::NEG_INFINITY
    };
    MultiToneMeasure {
        gain_db_per_freq: bins,
        noise_floor_dbfs: noise_floor,
    }
}

// --- Level sweep (AM-AM + AM-PM + P1dB + sweet spot) ---------------------

/// Analyse a tone level sweep: measure amplitude + phase at each segment
/// (using `tone_freq_hz` as the known carrier), build AM-AM and AM-PM
/// curves, then find the 1 dB compression point and the recommended
/// sweet spot (P1dB − 3 dB).
///
/// `stamps` must be sorted from highest level (closest to 0 dBFS) down,
/// or the P1dB walk-down logic will not find the compression edge. The
/// orchestrator (`sounder.rs`) emits them in this order by default.
pub fn measure_level_sweep(
    audio: &[f32],
    sr: u32,
    stamps: &[LevelStamp],
    tone_freq_hz: f64,
) -> LevelSweepMeasure {
    let mut samples: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(stamps.len());
    // For each segment: skip the first/last 10 ms (fade), measure tone amp/phase + SNR.
    let fade = (0.012 * sr as f64) as usize;
    for s in stamps {
        let start = s.start_sample + fade;
        let end = s.end_sample.saturating_sub(fade);
        if end <= start {
            continue;
        }
        let seg = &audio[start..end];
        let m = measure_tone(seg, sr, tone_freq_hz);
        let out_dbfs = 20.0 * (m.amplitude.max(1e-20)).log10();
        samples.push((s.level_db, out_dbfs, m.phase_rad, m.snr_db));
    }
    // Ascending input order for AM-AM.
    samples.sort_by(|a, b| {
        a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    let am_am: Vec<(f32, f32)> =
        samples.iter().map(|t| (t.0, t.1)).collect();
    // Reference phase = lowest-level segment (least distorted).
    let ref_phase = samples
        .first()
        .map(|t| t.2)
        .unwrap_or(0.0);
    let am_pm: Vec<(f32, f32)> = samples
        .iter()
        .map(|t| {
            // Wrap difference into (-π, π]
            let mut d = t.2 - ref_phase;
            while d > std::f32::consts::PI {
                d -= 2.0 * std::f32::consts::PI;
            }
            while d <= -std::f32::consts::PI {
                d += 2.0 * std::f32::consts::PI;
            }
            (t.0, d)
        })
        .collect();
    let snr: Vec<(f32, f32)> =
        samples.iter().map(|t| (t.0, t.3)).collect();

    // P1dB: take the lowest two levels as the small-signal slope
    // reference, then walk upward and find the first level where the
    // measured output falls 1 dB below the linear extrapolation.
    let p1db = if am_am.len() >= 3 {
        let (xa, ya) = am_am[0];
        let (xb, yb) = am_am[1];
        let slope = if (xb - xa).abs() > 1e-3 {
            (yb - ya) / (xb - xa)
        } else {
            1.0
        };
        let intercept = ya - slope * xa;
        let mut found = f32::NAN;
        for &(x, y) in &am_am[2..] {
            let linear = slope * x + intercept;
            if (linear - y) >= 1.0 {
                found = x;
                break;
            }
        }
        found
    } else {
        f32::NAN
    };
    // Sweet spot: P1dB − 3 dB, clamped to the observed input range.
    let sweet = if p1db.is_finite() && !am_am.is_empty() {
        let lo = am_am.first().map(|t| t.0).unwrap_or(p1db);
        (p1db - 3.0).max(lo)
    } else if !samples.is_empty() {
        // No compression observed in the sweep: pick the highest-SNR
        // segment as a fallback recommendation.
        samples
            .iter()
            .max_by(|a, b| {
                a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|t| t.0)
            .unwrap_or(f32::NAN)
    } else {
        f32::NAN
    };
    LevelSweepMeasure {
        am_am_curve: am_am,
        am_pm_curve: am_pm,
        p1db_dbfs: p1db,
        sweet_spot_dbfs: sweet,
        snr_db_per_level: snr,
    }
}

// --- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{awgn, chirp_linear, level_sweep, multitone, tone, two_tone};
    use crate::types::AUDIO_RATE;

    use crate::probe::{sync_marker, wake_up_tone};

    #[test]
    fn find_sync_marker_locks_on_known_offset() {
        // Build a realistic capture: band noise, wake-up tone, sync,
        // gap, then trailing noise. Verify the finder locates the
        // sync chirp at the exact known sample offset.
        let template = sync_marker(0.7);
        let pre_noise = awgn(0.8, 0.05, 11); // 0.8 s of band noise
        let wake = wake_up_tone(0.6); // 1.5 s
        let gap = crate::probe::silence(0.2);
        let probe = tone(1500.0, 0.5, 0.5);
        let trail = awgn(0.3, 0.05, 13);
        let mut capture = Vec::new();
        capture.extend(&pre_noise);
        capture.extend(&wake);
        let sync_start = capture.len();
        capture.extend(&template);
        capture.extend(&gap);
        capture.extend(&probe);
        capture.extend(&trail);
        let found = find_sync_marker(&capture, &template, 6.0)
            .expect("sync marker should be found");
        // Allow up to ±5 samples (≈100 µs) of slop from the corr peak.
        let dev = (found as i64 - sync_start as i64).abs();
        assert!(
            dev <= 5,
            "sync at {found}, expected {sync_start}, dev {dev}",
        );
    }

    #[test]
    fn find_sync_marker_returns_none_on_pure_noise() {
        let template = sync_marker(0.7);
        let noise = awgn(3.0, 0.1, 99);
        let res = find_sync_marker(&noise, &template, 6.0);
        assert!(res.is_none(), "sync should not lock on pure noise");
    }

    #[test]
    fn hilbert_pure_tone_has_constant_magnitude() {
        let audio = tone(1500.0, 0.2, 0.7);
        let z = hilbert_fft(&audio);
        // Skip fade zones and DC ramp at the analytic-signal edges.
        let skip = audio.len() / 5;
        let mid = &z[skip..z.len() - skip];
        let mags: Vec<f64> = mid.iter().map(|c| (c.re * c.re + c.im * c.im).sqrt()).collect();
        let mean: f64 = mags.iter().sum::<f64>() / mags.len() as f64;
        let max_dev = mags.iter().map(|m| (m - mean).abs()).fold(0.0_f64, f64::max);
        // Expect magnitude ≈ 0.7 with small ripple from the FFT edges.
        assert!(
            (mean - 0.7).abs() < 0.02,
            "mean mag {mean} expected ≈ 0.7",
        );
        assert!(
            max_dev / mean < 0.02,
            "ripple {} / {} too large",
            max_dev,
            mean,
        );
    }

    #[test]
    fn measure_tone_recovers_amplitude_and_phase() {
        // Skip the fade zones (10 ms at each end) so we measure on the
        // steady-state body of the tone.
        let audio = tone(1500.0, 0.5, 0.7);
        let fade = (0.010 * AUDIO_RATE as f64) as usize;
        let m = measure_tone(&audio[fade..audio.len() - fade], AUDIO_RATE, 1500.0);
        assert!((m.amplitude - 0.7).abs() < 0.005, "amp {}", m.amplitude);
        // SNR should be very high (no noise).
        assert!(m.snr_db > 60.0, "snr {} too low", m.snr_db);
    }

    #[test]
    fn measure_tone_snr_drops_on_added_noise() {
        let mut audio = tone(1500.0, 0.5, 0.7);
        let noise = awgn(0.5, 0.05, 1);
        for (a, n) in audio.iter_mut().zip(noise.iter()) {
            *a += *n;
        }
        // Strip fade zones before measurement (the fade ramps would
        // otherwise wash out the steady-state SNR by mixing zero
        // signal with noise).
        let fade = (0.010 * AUDIO_RATE as f64) as usize;
        let m = measure_tone(&audio[fade..audio.len() - fade], AUDIO_RATE, 1500.0);
        // Signal power: 0.7²/2 = 0.245. Noise power: 0.05² = 0.0025.
        // SNR ≈ 10·log10(0.245 / 0.0025) ≈ 19.9 dB.
        assert!(
            (m.snr_db - 19.9).abs() < 1.5,
            "snr {} expected ≈ 19.9",
            m.snr_db,
        );
    }

    #[test]
    fn measure_two_tone_clean_chain_has_deep_imd3() {
        // Pure two-tone + tiny noise → IMD3 should be very far below.
        let mut audio = two_tone(700.0, 1900.0, 0.5, 0.4);
        let noise = awgn(0.5, 1e-4, 7);
        for (a, n) in audio.iter_mut().zip(noise.iter()) {
            *a += *n;
        }
        let m = measure_two_tone(&audio, AUDIO_RATE, 700.0, 1900.0);
        assert!(m.imd3_low_dbc < -40.0, "imd3 low {}", m.imd3_low_dbc);
        assert!(m.imd3_high_dbc < -40.0, "imd3 high {}", m.imd3_high_dbc);
    }

    #[test]
    fn measure_two_tone_detects_synthetic_imd3() {
        // Inject a third raie at 2f1-f2 = 2·700-1900 = -500 -> alias at 500 Hz.
        // Pick non-aliasing freqs instead: f1=700, f2=900 -> 2f1-f2=500, 2f2-f1=1100.
        let mut audio = two_tone(700.0, 900.0, 0.5, 0.4);
        let imd_inject = tone(500.0, 0.5, 0.04); // -20 dBc vs 0.4
        for (a, b) in audio.iter_mut().zip(imd_inject.iter()) {
            *a += *b;
        }
        let m = measure_two_tone(&audio, AUDIO_RATE, 700.0, 900.0);
        // imd3_low_dbc compares to a1; with a1 ≈ 0.4 and IMD ≈ 0.04, dBc ≈ -20.
        assert!(
            (m.imd3_low_dbc - (-20.0)).abs() < 1.0,
            "imd3_low_dbc {} expected ≈ -20",
            m.imd3_low_dbc,
        );
    }

    #[test]
    fn measure_chirp_flat_channel_has_small_group_delay_deviation() {
        let audio = chirp_linear(200.0, 2700.0, 1.0, 0.7);
        let m = measure_chirp(&audio, AUDIO_RATE, 200.0, 2700.0);
        assert_eq!(m.group_delay_per_freq.len(), 32);
        // No channel applied → group delay deviation is small. The
        // bound is loose (1 ms) because numerical IF estimation has
        // residual noise even after smoothing; what matters for the
        // measurement to be useful is that a real channel will show
        // 5-15 ms deviation across the audio LPF roll-off zone, well
        // above this floor.
        let max_abs = m
            .group_delay_per_freq
            .iter()
            .map(|(_, d)| d.abs())
            .fold(0.0_f32, f32::max);
        assert!(max_abs < 1000.0, "gd dev {} too large", max_abs);
        // BW recovered should cover most of the chirp range.
        assert!(
            m.bw_3db_hz.0 < 600.0 && m.bw_3db_hz.1 > 2300.0,
            "bw {:?} suspicious",
            m.bw_3db_hz,
        );
    }

    #[test]
    fn measure_multitone_recovers_each_bin() {
        let freqs = [500.0, 1000.0, 1500.0, 2000.0, 2500.0];
        let audio = multitone(&freqs, 0.5, 0.15);
        let m = measure_multitone(&audio, AUDIO_RATE, &freqs);
        assert_eq!(m.gain_db_per_freq.len(), 5);
        for &(_, g) in &m.gain_db_per_freq {
            assert!(g > -3.0, "freq gain {} too low", g);
        }
        // Noise floor should be way below 0 dBFS (no noise injected).
        assert!(m.noise_floor_dbfs < -40.0, "noise_floor {}", m.noise_floor_dbfs);
    }

    #[test]
    fn measure_level_sweep_linear_chain() {
        let levels = [0.0_f32, -3.0, -6.0, -9.0, -12.0];
        let (audio, stamps) =
            level_sweep(|amp| tone(1500.0, 0.4, amp), &levels, 0.15);
        let m = measure_level_sweep(&audio, AUDIO_RATE, &stamps, 1500.0);
        assert_eq!(m.am_am_curve.len(), 5);
        // No compression -> P1dB NaN; sweet spot = highest-SNR level.
        assert!(m.p1db_dbfs.is_nan(), "p1db {} should be NaN", m.p1db_dbfs);
        // AM-AM slope ≈ 1 (input dB → output dB).
        let (x0, y0) = m.am_am_curve[0];
        let (xN, yN) = *m.am_am_curve.last().unwrap();
        let slope = (yN - y0) / (xN - x0);
        assert!((slope - 1.0).abs() < 0.05, "slope {} ≠ 1", slope);
    }

    #[test]
    fn measure_level_sweep_recovers_p1db_with_synthetic_compression() {
        // Simulate a chain that compresses input above -3 dBFS: output
        // = input − max(0, (input - (-3)) · 0.5) in dB.
        let levels = [0.0_f32, -1.0, -2.0, -3.0, -4.0, -6.0, -9.0, -12.0];
        let mut all_audio = Vec::new();
        let mut stamps = Vec::new();
        for &level_db in &levels {
            let amp_in = 10.0_f32.powf(level_db / 20.0);
            let raw = tone(1500.0, 0.4, amp_in);
            let mut out_db = level_db;
            if level_db > -3.0 {
                out_db -= 0.5 * (level_db - (-3.0));
            }
            let scale = 10.0_f32.powf((out_db - level_db) / 20.0);
            let start = all_audio.len();
            all_audio.extend(raw.iter().map(|&x| x * scale));
            let end = all_audio.len();
            stamps.push(LevelStamp { start_sample: start, end_sample: end, level_db });
            // gap
            all_audio.extend(std::iter::repeat(0.0_f32).take(7200));
        }
        let m = measure_level_sweep(&all_audio, AUDIO_RATE, &stamps, 1500.0);
        // P1dB should fall in the range where compression has lifted
        // the linear-extrapolation gap to ≥ 1 dB.  With the 0.5
        // compression coeff, linear_y - measured_y = 0.5·(x - (-3)).
        // = 1 at x = -1 dBFS. Tolerate ±1 dB resolution.
        assert!(
            m.p1db_dbfs.is_finite() && (m.p1db_dbfs - (-1.0)).abs() < 1.5,
            "p1db {} expected ≈ -1",
            m.p1db_dbfs,
        );
        // Sweet spot = P1dB − 3 dB ≈ -4 dBFS.
        assert!(
            (m.sweet_spot_dbfs - (m.p1db_dbfs - 3.0)).abs() < 0.1,
            "sweet {} vs p1db {}",
            m.sweet_spot_dbfs,
            m.p1db_dbfs,
        );
    }
}
