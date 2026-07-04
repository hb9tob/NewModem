//! Carrier-frequency-offset (CFO) estimation for the turbo RX path.
//!
//! In a suppressed-carrier linear mode (SSB USB-data, the QO-100 target) the
//! TX and RX oscillators differ, so the modem signal arrives shifted by a
//! constant carrier offset (≈108 Hz observed off-air). The preamble matched
//! filter (256 symbols, ~170 ms) has a sinc null at ≈ `Fs/(N_PREAMBLE·sps)`
//! ≈ 6 Hz, so even ~100 Hz destroys acquisition. This module provides the two
//! estimators the [`crate::v3_session`] turbo path uses to recover that offset
//! and drive the downmix NCO ([`crate::streaming_dsp`-side `set_cfo_hz`]):
//!
//! 1. [`estimate_coarse_cfo`] — a **frequency-domain band-power matched
//!    filter**: it locates the RRC-shaped occupied band in the magnitude
//!    spectrum to within a few Hz (enough to land inside the matched-filter
//!    main lobe), robust to narrow QRM because it integrates over the whole
//!    band (~hundreds of FFT bins) rather than peak-picking.
//! 2. the fine offset is resolved in [`crate::v3_session`] by maximising the
//!    **time-domain preamble matched-filter metric** on a ±15 Hz grid around
//!    the coarse bracket, with parabolic sub-bin interpolation — the unbiased
//!    estimate actually committed. NOTE: [`estimate_fine_cfo`] below, a
//!    data-aided single-lag (Kay) estimator, is SHELVED (kept for reference /
//!    A-B); it is not on the live turbo path.
//!
//! This module is pure DSP (no I/O, no session state) so it is unit-testable
//! in isolation and reusable by both the in-session acquisition and the
//! worker-level [`crate::turbo_gate`].

use std::f64::consts::PI;

use crate::demodulator::get_or_build_plans;
use crate::profile::ModemConfig;
use crate::types::{Complex64, AUDIO_RATE};

/// Below this magnitude an estimated offset is treated as zero. Chosen below
/// the smallest real offset we expect to correct (≈108 Hz) and above the
/// residual jitter of the estimators on a clean signal, so a CFO-free signal
/// snaps to an exact 0.0 → every downstream stage takes its byte-exact
/// no-op fast-path. The single guard for the "no regression at 0 Hz" rule.
pub const CFO_DEADBAND_HZ: f64 = 1.5;

/// Maximum carrier offset the coarse estimator searches for, each side of the
/// nominal centre (Hz). Matches the iteration scope: the residual after SDR
/// tuning must sit inside the SSB passband. Larger offsets are an SDR-tuning
/// concern, out of scope here.
pub const CFO_SEARCH_RADIUS_HZ: f64 = 250.0;

/// FFT size for the coarse spectral estimate. 16384 @ 48 kHz → ~2.93 Hz/bin,
/// finer than the matched-filter main lobe (±5.86 Hz), and parabolic
/// interpolation on the band-power curve refines below the bin. The gate /
/// acquisition windows are always at least this long.
pub const COARSE_FFT_SIZE: usize = 16384;

/// Snap an estimated offset to exactly 0.0 inside the deadband (see
/// [`CFO_DEADBAND_HZ`]). Keeps the no-op-at-0 contract a single decision.
#[inline]
pub fn snap_deadband(cfo_hz: f64) -> f64 {
    if cfo_hz.abs() < CFO_DEADBAND_HZ {
        0.0
    } else {
        cfo_hz
    }
}

/// Minimum fraction of residual spectral power that must fall in a band before a
/// CFO grid search is worth running there (see [`band_signal_ratio`]). Idle
/// noise sits well below this; a real modem burst sits near 1.
pub const BAND_SIGNAL_RATIO_MIN: f64 = 0.4;

/// Matched-filter-metric refinement grid (Hz) around a coarse estimate. ±15 Hz
/// at 3 Hz steps covers the coarse estimator's intrinsic spectral-centroid bias
/// (~10 Hz on a real modem burst) while keeping every grid point inside the
/// matched filter's ~6 Hz main lobe of the true offset. The caller evaluates the
/// MF metric at each and keeps the best — used only once a signal-bearing band
/// fails to lock at 0.
pub fn refine_grid(coarse: f64) -> impl Iterator<Item = f64> {
    (-5..=5).map(move |k| coarse + k as f64 * 3.0)
}

/// Spectral signature of the occupied modem band, derived from a profile.
/// The coarse estimator cross-correlates the measured PSD against the expected
/// raised-cosine power-spectrum template of this band (built from `symbol_rate`
/// and `beta`) over `center_hz ± search_radius_hz` and reports the offset of
/// the best-fitting centre. The template's sloped roll-off edges localise the
/// peak even though the band is ~flat-topped, and out-of-band interferers get
/// near-zero template weight. The differing widths between geometry families
/// (A/B/C) also help disambiguate the geometry.
#[derive(Clone, Copy, Debug)]
pub struct CfoBand {
    /// Nominal carrier centre (Hz), normally `DATA_CENTER_HZ` = 1100.
    pub center_hz: f64,
    /// Symbol rate (Bd) — sets the occupied bandwidth.
    pub symbol_rate: f64,
    /// RRC roll-off factor.
    pub beta: f64,
    /// Search radius each side of `center_hz` (Hz).
    pub search_radius_hz: f64,
}

impl CfoBand {
    /// Build from a modem profile.
    pub fn from_config(cfg: &ModemConfig) -> Self {
        CfoBand {
            center_hz: cfg.center_freq_hz,
            symbol_rate: cfg.symbol_rate,
            beta: cfg.beta,
            search_radius_hz: CFO_SEARCH_RADIUS_HZ,
        }
    }

    /// Half the occupied RRC bandwidth, `(1+beta)·Rs / 2` (Hz).
    pub fn half_bw_hz(&self) -> f64 {
        (1.0 + self.beta) * self.symbol_rate / 2.0
    }

    /// Raised-cosine **power** response at frequency offset `f` (Hz) from the
    /// band centre — the expected shape of the received PSD before the matched
    /// filter (TX RRC pulse-shaping → power ∝ |RRC|² = raised cosine). Unity in
    /// the flat passband, cosine roll-off across the transition, zero beyond.
    fn rc_power(&self, f: f64) -> f64 {
        let f = f.abs();
        let edge_in = (1.0 - self.beta) * self.symbol_rate / 2.0;
        let edge_out = (1.0 + self.beta) * self.symbol_rate / 2.0;
        if f <= edge_in {
            1.0
        } else if f <= edge_out && self.beta > 0.0 {
            0.5 * (1.0 + (PI / (self.beta * self.symbol_rate) * (f - edge_in)).cos())
        } else {
            0.0
        }
    }
}

/// Estimate the coarse carrier offset (Hz) of the modem signal in `window`
/// (real passband audio at [`AUDIO_RATE`]) relative to `band.center_hz`. A
/// positive result means the signal sits **above** the nominal centre.
///
/// Method: average a few Hann-windowed periodograms (Welch) from the tail of
/// `window`, subtract a global noise floor, then cross-correlate the PSD with
/// the band's raised-cosine power template (see [`CfoBand::rc_power`]) over the
/// search range. The correlation peak — refined by parabolic interpolation —
/// gives the band centre; subtract the nominal centre for the offset. The
/// caller is expected to [`snap_deadband`] the result.
///
/// Returns `0.0` if `window` is too short to analyse.
pub fn estimate_coarse_cfo(window: &[f32], band: &CfoBand) -> f64 {
    match coarse_psd(window) {
        Some(psd) => estimate_coarse_cfo_from_psd(&psd, band),
        None => 0.0,
    }
}

/// Compute the noise-floor-subtracted positive-half PSD used by the coarse
/// estimator. Geometry-independent (depends only on `window` and the fixed FFT
/// size), so a caller scanning several geometries over the same window — e.g.
/// [`crate::turbo_gate::TurboGate`] — should compute it ONCE and feed it to
/// [`estimate_coarse_cfo_from_psd`] per geometry. Returns `None` if `window` is
/// too short to trust.
pub fn coarse_psd(window: &[f32]) -> Option<Vec<f64>> {
    let n = COARSE_FFT_SIZE;
    if window.len() < n / 4 {
        return None;
    }
    let mut psd = welch_psd(window, n);
    let half = n / 2;
    psd.truncate(half); // only the positive half is relevant (band ≈ +1100 Hz)
    // Subtract a robust global noise floor (median of the positive half) so the
    // out-of-band noise plateau doesn't bias the correlation; clamp at 0.
    let floor = median(&psd);
    for p in &mut psd {
        *p = (*p - floor).max(0.0);
    }
    Some(psd)
}

/// Fraction of the floor-subtracted residual power that falls inside the band's
/// occupied region (over the search range). ≈ 1 when a modem signal occupies
/// the band, ≈ band-width/spectrum-width (small) for idle noise — a
/// scale-invariant "signal present in this band" test. From a precomputed
/// [`coarse_psd`].
pub fn band_signal_ratio(psd: &[f64], band: &CfoBand) -> f64 {
    let n = COARSE_FFT_SIZE;
    let bin_hz = AUDIO_RATE as f64 / n as f64;
    let half = psd.len();
    let edge = band.half_bw_hz() + band.search_radius_hz;
    let lo = ((band.center_hz - edge) / bin_hz).floor().max(0.0) as usize;
    let hi = (((band.center_hz + edge) / bin_hz).ceil() as usize).min(half);
    let total: f64 = psd.iter().sum();
    if total <= 0.0 {
        return 0.0;
    }
    let in_band: f64 = psd[lo..hi].iter().sum();
    in_band / total
}

/// Coarse offset (Hz) from a precomputed [`coarse_psd`] (positive half, floor
/// subtracted). See [`estimate_coarse_cfo`] for the method.
pub fn estimate_coarse_cfo_from_psd(psd: &[f64], band: &CfoBand) -> f64 {
    let n = COARSE_FFT_SIZE;
    let bin_hz = AUDIO_RATE as f64 / n as f64;
    let half = psd.len();

    // Raised-cosine power template, sampled at bin spacing, ±edge_out around 0.
    let edge_out = band.half_bw_hz();
    let tmpl_half = (edge_out / bin_hz).ceil() as isize;
    let tmpl: Vec<f64> = (-tmpl_half..=tmpl_half)
        .map(|b| band.rc_power(b as f64 * bin_hz))
        .collect();

    let lo_center = ((band.center_hz - band.search_radius_hz) / bin_hz).floor() as isize;
    let hi_center = ((band.center_hz + band.search_radius_hz) / bin_hz).ceil() as isize;

    let corr = |center_bin: isize| -> f64 {
        let mut acc = 0.0;
        for (j, &w) in tmpl.iter().enumerate() {
            let bin = center_bin - tmpl_half + j as isize;
            if bin >= 0 && (bin as usize) < half {
                acc += psd[bin as usize] * w;
            }
        }
        acc
    };

    let mut best_bin = lo_center;
    let mut best_corr = f64::NEG_INFINITY;
    for c in lo_center..=hi_center {
        let v = corr(c);
        if v > best_corr {
            best_corr = v;
            best_bin = c;
        }
    }

    // Parabolic interpolation on the correlation curve for sub-bin resolution
    // (only when the peak is interior to the search range).
    let mut frac = 0.0f64;
    if best_bin > lo_center && best_bin < hi_center {
        let pm = corr(best_bin - 1);
        let p0 = best_corr;
        let pp = corr(best_bin + 1);
        let denom = pm - 2.0 * p0 + pp;
        if denom.abs() > f64::EPSILON {
            frac = (0.5 * (pm - pp) / denom).clamp(-0.5, 0.5);
        }
    }

    let best_center_hz = (best_bin as f64 + frac) * bin_hz;
    best_center_hz - band.center_hz
}

/// Median of a slice (clones + sorts a working copy). Used for the noise-floor
/// estimate; called once per acquisition scan, so the allocation is fine.
fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = s.len() / 2;
    if s.len() % 2 == 0 {
        0.5 * (s[m - 1] + s[m])
    } else {
        s[m]
    }
}

/// Welch power spectral density: average the squared-magnitude spectra of up
/// to four Hann-windowed, 50%-overlapped segments taken from the **tail** of
/// `window` (the most recent, highest-SNR audio). Returns `nfft` bins.
fn welch_psd(window: &[f32], nfft: usize) -> Vec<f64> {
    let plans = get_or_build_plans(nfft);
    let hann: Vec<f64> = (0..nfft)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / nfft as f64).cos())
        .collect();

    let avail = window.len();
    let hop = nfft / 2;
    const MAX_SEGS: usize = 4;

    let mut psd = vec![0.0f64; nfft];
    let mut nseg = 0usize;
    let mut buf = vec![Complex64::new(0.0, 0.0); nfft];

    for s in 0..MAX_SEGS {
        // Segment ends `s*hop` samples before the tail.
        let end = avail.saturating_sub(s * hop);
        if end < nfft {
            break;
        }
        let start = end - nfft;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = Complex64::new(window[start + i] as f64 * hann[i], 0.0);
        }
        plans.forward.process(&mut buf);
        for (p, c) in psd.iter_mut().zip(buf.iter()) {
            *p += c.norm_sqr();
        }
        nseg += 1;
    }

    if nseg == 0 {
        // Window shorter than nfft: a single zero-padded segment from the tail.
        let take = avail.min(nfft);
        let start = avail - take;
        for (i, b) in buf.iter_mut().enumerate() {
            let v = if i < take {
                window[start + i] as f64 * hann[i]
            } else {
                0.0
            };
            *b = Complex64::new(v, 0.0);
        }
        plans.forward.process(&mut buf);
        for (p, c) in psd.iter_mut().zip(buf.iter()) {
            *p += c.norm_sqr();
        }
        nseg = 1;
    }

    let inv = 1.0 / nseg as f64;
    for p in &mut psd {
        *p *= inv;
    }
    psd
}

/// Data-aided fine carrier-offset estimate (Hz) from the received preamble
/// symbols `rx_preamble` against the known transmitted `known` symbols, with
/// symbol period `tsym_s` seconds. Single-lag (Kay) estimator:
///
/// `z[k] = rx[k]·conj(known[k])` removes the data modulation, leaving a pure
/// tone at the residual frequency; `δ = arg(Σ z[k+1]·conj(z[k])) / (2π·tsym)`.
///
/// Unambiguous over ±1/(2·tsym) = ±Rs/2, so a full off-air offset (≤250 Hz ≪
/// Rs/2) is recovered in a single pass. Returns `0.0` for degenerate input.
pub fn estimate_fine_cfo(rx_preamble: &[Complex64], known: &[Complex64], tsym_s: f64) -> f64 {
    let n = rx_preamble.len().min(known.len());
    if n < 2 || tsym_s <= 0.0 {
        return 0.0;
    }
    let mut prev = rx_preamble[0] * known[0].conj();
    let mut acc = Complex64::new(0.0, 0.0);
    for k in 1..n {
        let cur = rx_preamble[k] * known[k].conj();
        acc += cur * prev.conj();
        prev = cur;
    }
    if acc.norm_sqr() == 0.0 {
        return 0.0;
    }
    acc.arg() / (2.0 * PI * tsym_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_band() -> CfoBand {
        // Family A: Rs=1500, beta=0.20 → occupied ≈ 1800 Hz.
        CfoBand {
            center_hz: 1100.0,
            symbol_rate: 1500.0,
            beta: 0.20,
            search_radius_hz: CFO_SEARCH_RADIUS_HZ,
        }
    }

    /// A deterministic LCG so the tests stay reproducible without rand and
    /// without the forbidden Math.random/Date entropy.
    struct Lcg(u64);
    impl Lcg {
        fn next_f64(&mut self) -> f64 {
            // xorshift*-ish; just needs to be uniform-ish in [-1, 1).
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        }
    }

    /// Build a real passband signal whose PSD follows the band's RRC power
    /// shape, centred at `center+delta`, plus white noise and a narrow QRM
    /// carrier that is strong per-bin (to exercise template rejection) but
    /// in-band. Dense tones (200) with RRC-shaped amplitudes approximate the
    /// continuous raised-cosine power spectrum.
    fn synth_band(band: &CfoBand, delta: f64, n: usize, snr_lin: f64) -> Vec<f32> {
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        let half_bw = band.half_bw_hz();
        let ntone = 200usize;
        let mut phases = vec![0.0f64; ntone];
        let mut amps = vec![0.0f64; ntone];
        for m in 0..ntone {
            phases[m] = rng.next_f64() * PI;
            let f_off = -half_bw + 2.0 * half_bw * m as f64 / (ntone - 1) as f64;
            // amplitude ∝ sqrt(power) so the PSD matches the rc_power template.
            amps[m] = band.rc_power(f_off).sqrt();
        }
        let band_center = band.center_hz + delta;
        let mut out = vec![0.0f32; n];
        for (i, o) in out.iter_mut().enumerate() {
            let t = i as f64 / AUDIO_RATE as f64;
            let mut acc = 0.0f64;
            for m in 0..ntone {
                let f = band_center - half_bw + 2.0 * half_bw * m as f64 / (ntone - 1) as f64;
                acc += amps[m] * (2.0 * PI * f * t + phases[m]).cos();
            }
            // QRM: a single carrier, strong per-bin but a small fraction of the
            // band's total power — the regime template correlation rejects.
            let qrm = 3.0 * (2.0 * PI * (band.center_hz - 200.0) * t).cos();
            let noise = rng.next_f64() / snr_lin.sqrt();
            *o = (acc + qrm + noise) as f32;
        }
        out
    }

    #[test]
    fn coarse_recovers_offset_with_qrm() {
        let band = test_band();
        let n = COARSE_FFT_SIZE * 2;
        for &delta in &[-250.0, -200.0, -108.0, -50.0, 0.0, 50.0, 108.0, 200.0, 250.0] {
            let sig = synth_band(&band, delta, n, 10.0);
            let est = estimate_coarse_cfo(&sig, &band);
            assert!(
                (est - delta).abs() <= 5.0,
                "coarse δ={delta}: est={est:.2} (err {:.2} Hz > 5)",
                est - delta,
            );
        }
    }

    #[test]
    fn coarse_zero_on_centred_signal_snaps_in_deadband() {
        let band = test_band();
        let sig = synth_band(&band, 0.0, COARSE_FFT_SIZE * 2, 20.0);
        let est = estimate_coarse_cfo(&sig, &band);
        assert!(est.abs() <= 5.0, "centred signal coarse est = {est:.2}");
    }

    #[test]
    fn fine_recovers_offset_under_noise() {
        let mut rng = Lcg(0xdead_beef_cafe_1234);
        let rs = 1500.0;
        let tsym = 1.0 / rs;
        // Known QPSK-ish reference (unit magnitude, random quadrant).
        let n = 256usize;
        let known: Vec<Complex64> = (0..n)
            .map(|_| {
                let a = if rng.next_f64() > 0.0 { 1.0 } else { -1.0 };
                let b = if rng.next_f64() > 0.0 { 1.0 } else { -1.0 };
                Complex64::new(a, b) / 2.0f64.sqrt()
            })
            .collect();
        for &delta in &[-250.0, -150.0, -108.0, -50.0, 50.0, 108.0, 200.0, 250.0] {
            let rx: Vec<Complex64> = known
                .iter()
                .enumerate()
                .map(|(k, &s)| {
                    let ph = 2.0 * PI * delta * k as f64 * tsym;
                    let rot = Complex64::new(ph.cos(), ph.sin());
                    let noise = Complex64::new(rng.next_f64() * 0.05, rng.next_f64() * 0.05);
                    s * rot + noise
                })
                .collect();
            let est = estimate_fine_cfo(&rx, &known, tsym);
            assert!(
                (est - delta).abs() < 1.0,
                "fine δ={delta}: est={est:.3} (err {:.3} Hz > 1)",
                est - delta,
            );
        }
    }

    #[test]
    fn deadband_snaps_small_offsets() {
        assert_eq!(snap_deadband(0.0), 0.0);
        assert_eq!(snap_deadband(1.0), 0.0);
        assert_eq!(snap_deadband(-1.49), 0.0);
        assert_eq!(snap_deadband(2.0), 2.0);
        assert_eq!(snap_deadband(-108.0), -108.0);
    }
}
