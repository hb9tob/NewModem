//! FFT-based SOF presence probe for the 2x family.
//!
//! Sibling of [`modem_core::gate::PreambleProbe`] adapted to the V4 wire
//! format. Cheap idle gate (~25–30 ms per call on Pi5) that lets the
//! worker skip the symbol-domain SOF correlation + LDPC pipeline when
//! the audio buffer holds nothing but band noise.
//!
//! # Differences vs the V3 probe
//!
//! - **Template is the SOF only (64 sym Chu-5)** rather than the 256-sym
//!   random preamble. All 8 [`ProfileIndex2x`] entries route through
//!   [`PreambleFamily2x::C`] (see `profile2x::profile_*_2x`) so the
//!   probe builds three templates that differ only in `(sps, β)`:
//!
//!   | sps | β    | symbol_rate | anchor     | profile bucket            |
//!   |-----|------|------------:|------------|---------------------------|
//!   |  32 | 0.20 |     1500 Bd | Normal2x   | NORMAL/HIGH/HIGH+/HIGH++  |
//!   |  48 | 0.25 |     1000 Bd | Robust2x   | ROBUST                    |
//!   |  96 | 0.25 |      500 Bd | Ultra2x    | ULTRA                     |
//!
//!   PLS payload (`profile_index` byte, parsed by
//!   [`rx_v4_symbols`](crate::rx_v4::rx_v4_symbols)) refines the
//!   1500 Bd bucket into the actual profile downstream.
//!
//! - **64-sym template means ~12 dB less correlation gain than V3** (256
//!   → 64). Threshold tightens accordingly but the autocorrelation of a
//!   Chu sequence is by construction optimal — clean signal still gives
//!   ratios in the high hundreds to low thousands.
//!
//! # Cost
//!
//! One rFFT of the audio buffer + 3 × (spectral multiply + irFFT +
//! magnitude scan). At buf_len = [`IDLE_PROBE_BUF_SAMPLES`] (2 s at
//! 48 kHz), fft_len next-pow2 above `buf_len + max_template_len` ≈
//! 131 072 → ≈ 25 ms on Pi5.
//!
//! # Note
//!
//! Like [`modem_core::gate::PreambleProbe`], this is a **presence**
//! probe. A positive result only says "something looks like a Chu-5
//! SOF" — the caller still runs the full
//! [`rx_v4_symbols`](crate::rx_v4::rx_v4_symbols) pipeline to extract
//! PLS payload, AppHeader and codewords.

use std::sync::Arc;
use std::sync::OnceLock;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use modem_core_base::modulator;
use modem_core_base::rrc::rrc_taps;
use modem_core_base::types::{Complex64, AUDIO_RATE, DATA_CENTER_HZ, RRC_SPAN_SYM};

use crate::plheader::{sof_for_family, PreambleFamily2x, SOF_LEN_SYM};
use crate::profile2x::ProfileIndex2x;

/// One pre-computed conjugated FFT spectrum per (sps, β) family.
///
/// All 8 [`ProfileIndex2x`] entries share the same family-C Chu-5 SOF.
/// They split by `(sps, β)` into three pulse-shaped templates:
///
/// - `(sps=32, β=0.20)` → Normal2x anchor. NORMAL/HIGH/HIGH+/HIGH++ /
///   HIGH56/HIGH+56 all share these RRC knobs; PLS refines to the exact
///   profile after decode.
/// - `(sps=48, β=0.25)` → Robust2x anchor (unambiguous).
/// - `(sps=96, β=0.25)` → Ultra2x anchor (unambiguous).
const PROBE_TEMPLATES_2X: &[(usize, f64, ProfileIndex2x)] = &[
    (32, 0.20, ProfileIndex2x::Normal2x), // NORMAL/HIGH/HIGH+/HIGH++/HIGH56/HIGH+56
    (48, 0.25, ProfileIndex2x::Robust2x),
    (96, 0.25, ProfileIndex2x::Ultra2x),
];

/// Peak² / mean² ratio above which the probe declares an SOF likely
/// present.
///
/// Theoretical noise baseline: random Gaussian noise of length L has
/// expected peak² / mean² ≈ 2·ln(L), and taking the max across our 3
/// templates inflates this by another ≈ ln(3). At fft_len = 131 072
/// that's ≈ 25–30 in expectation. Empirically (16 RNG seeds, white
/// Gaussian, -26 dBFS) the worst-case observed is ≈ 65 — the RRC
/// pulse shaping concentrates the templates' spectra in the data
/// band, so any in-band noise lobe correlates more efficiently than
/// the white-noise model predicts.
///
/// Clean preamble: the SOF auto-correlation produces ratios in the
/// high hundreds to low thousands across all 8 profiles (see
/// `snr_sweep_monotonic_and_robust`). The 120 threshold gives ~1.8×
/// margin over the empirical noise worst-case AND ≥ 2× under the
/// clean-signal ratio for every profile.
pub const PROBE_THRESHOLD_2X: f64 = 120.0;

/// Pre-built FFT plans + per-template conjugated spectra. Constructed
/// once per buffer length via [`PreambleProbe2x::for_buf_len`] and
/// reused for every scan tick.
pub struct PreambleProbe2x {
    fft_len: usize,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    /// One conjugated spectrum per [`PROBE_TEMPLATES_2X`] entry.
    /// Length = `fft_len/2 + 1`.
    templates: Vec<Vec<Complex32>>,
    anchors: Vec<ProfileIndex2x>,
}

#[derive(Debug, Clone)]
pub struct ProbeResult2x {
    /// Best peak² / mean² ratio observed across all templates.
    pub max_ratio: f64,
    /// Anchor [`ProfileIndex2x`] of the template that produced
    /// `max_ratio`. The worker can use this as a coarse hint to switch
    /// between auto-detect candidates (Ultra/Robust/Normal); PLS payload
    /// of the matching cycle refines the 1500 Bd bucket later.
    pub best_anchor: ProfileIndex2x,
    /// All templates' ratios, in declaration order — matches
    /// [`PROBE_TEMPLATES_2X`] (Normal, Robust, Ultra).
    pub per_template_ratio: Vec<f64>,
}

impl ProbeResult2x {
    pub fn passes(&self, threshold: f64) -> bool {
        self.max_ratio >= threshold
    }
}

impl PreambleProbe2x {
    /// Get a process-wide cached probe sized for `buf_len`. The first
    /// call builds the plan + templates (~5 ms one-shot); subsequent
    /// calls return the same instance.
    ///
    /// The 2x worker only ever calls with one `buf_len` value (the
    /// [`IDLE_PROBE_BUF_SAMPLES`] constant), so a single `OnceLock`
    /// suffices.
    pub fn for_buf_len(buf_len: usize) -> &'static Self {
        static CACHE: OnceLock<PreambleProbe2x> = OnceLock::new();
        CACHE.get_or_init(|| Self::new(buf_len))
    }

    /// Constructor — builds FFT plans and per-template conjugated
    /// spectra. Public so unit tests can build instances at non-default
    /// buffer lengths; production code should use
    /// [`PreambleProbe2x::for_buf_len`].
    pub fn new(buf_len: usize) -> Self {
        // FFT length = next power of 2 above `buf_len + max_template_len`,
        // so the linear (non-circular) cross-correlation fits without
        // wrap-around. The Ultra template is the longest:
        //   SOF_LEN_SYM × sps + RRC_SPAN_SYM × sps
        //   = 64 × 96 + 12 × 96 = 7296 samples.
        let max_template_len = SOF_LEN_SYM * 96 + RRC_SPAN_SYM * 96;
        let fft_len = next_pow2(buf_len + max_template_len);

        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(fft_len);
        let inverse = planner.plan_fft_inverse(fft_len);

        // Family-C SOF, shared by every ProfileIndex2x entry. Build the
        // 64-sym Chu-5 sequence once and clone references into each
        // template's pulse shaper.
        let sof_syms: Vec<Complex64> = sof_for_family(PreambleFamily2x::C).to_vec();

        let mut templates = Vec::with_capacity(PROBE_TEMPLATES_2X.len());
        let mut anchors = Vec::with_capacity(PROBE_TEMPLATES_2X.len());
        for &(sps, beta, anchor) in PROBE_TEMPLATES_2X {
            // Modulator-built passband template — identical pulse-shaping
            // chain to what the TX emits, so the correlation peak lands
            // at the actual SOF position byte-for-byte (within rounding).
            // pitch == sps (Nyquist tau = 1.0) for every 2x profile.
            let taps = rrc_taps(beta, RRC_SPAN_SYM, sps);
            let template =
                modulator::modulate(&sof_syms, sps, sps, &taps, DATA_CENTER_HZ);

            // Energy-normalise the template so cross-template ratios are
            // directly comparable: modulator::modulate peak-normalises,
            // which biases templates with denser pulse overlap (low sps)
            // toward smaller per-pulse amplitude. Without this, the
            // sps=32 template's ratio underestimates against the wider
            // sps=96 one even on identical SNR.
            let energy: f64 = template.iter().map(|&x| (x as f64) * (x as f64)).sum();
            let scale = if energy > 1e-12 {
                1.0 / energy.sqrt() as f32
            } else {
                1.0
            };

            // Zero-pad to fft_len, forward FFT, conjugate (so multiplying
            // audio_fft × this gives the cross-correlation spectrum in a
            // single elementwise step).
            let mut input = vec![0.0f32; fft_len];
            for (i, &x) in template.iter().enumerate() {
                input[i] = x * scale;
            }
            let mut spec = forward.make_output_vec();
            forward
                .process(&mut input, &mut spec)
                .expect("rFFT forward (template)");
            for c in &mut spec {
                *c = c.conj();
            }
            templates.push(spec);
            anchors.push(anchor);
        }

        PreambleProbe2x {
            fft_len,
            forward,
            inverse,
            templates,
            anchors,
        }
    }

    pub fn fft_len(&self) -> usize {
        self.fft_len
    }

    pub fn anchors(&self) -> &[ProfileIndex2x] {
        &self.anchors
    }

    /// Run the probe against `samples`. Cost per call ≈ 1 forward rFFT
    /// + N_TEMPLATES × (multiply + inverse rFFT + |y|² scan).
    ///
    /// `samples.len()` must be ≤ [`fft_len`](Self::fft_len). If shorter,
    /// the audio is zero-padded to fill the FFT.
    pub fn check(&self, samples: &[f32]) -> ProbeResult2x {
        // 1. Forward rFFT of audio (zero-padded if needed).
        let mut input = vec![0.0f32; self.fft_len];
        let n = samples.len().min(self.fft_len);
        input[..n].copy_from_slice(&samples[..n]);
        let mut audio_spec = self.forward.make_output_vec();
        self.forward
            .process(&mut input, &mut audio_spec)
            .expect("rFFT forward (audio)");

        // 2. Per-template cross-correlation: multiply spectra → iFFT →
        //    peak² / mean² ratio of the time-domain output.
        let spec_len = audio_spec.len();
        let mut work_spec = vec![Complex32::new(0.0, 0.0); spec_len];
        let mut output = vec![0.0f32; self.fft_len];

        let mut per_template_ratio = Vec::with_capacity(self.templates.len());
        let mut best_ratio = 0.0_f64;
        let mut best_anchor = self.anchors[0];

        for (idx, template_spec) in self.templates.iter().enumerate() {
            for ((s, &a), &t) in work_spec
                .iter_mut()
                .zip(audio_spec.iter())
                .zip(template_spec.iter())
            {
                *s = a * t;
            }
            self.inverse
                .process(&mut work_spec, &mut output)
                .expect("rFFT inverse");

            let mut max_sqr = 0.0_f64;
            let mut sum_sqr = 0.0_f64;
            for &y in &output {
                let v = (y as f64) * (y as f64);
                if v > max_sqr {
                    max_sqr = v;
                }
                sum_sqr += v;
            }
            let mean_sqr = sum_sqr / output.len() as f64;
            let ratio = if mean_sqr > 0.0 { max_sqr / mean_sqr } else { 0.0 };
            per_template_ratio.push(ratio);
            if ratio > best_ratio {
                best_ratio = ratio;
                best_anchor = self.anchors[idx];
            }
        }

        ProbeResult2x {
            max_ratio: best_ratio,
            best_anchor,
            per_template_ratio,
        }
    }
}

fn next_pow2(n: usize) -> usize {
    let mut p = 1usize;
    while p < n {
        p <<= 1;
    }
    p
}

/// Default audio buffer length the worker keeps for the probe — 2 s at
/// 48 kHz. Long enough to contain at least one PLHEADER (192 sym × max
/// 96 sps = 18432 samples ≈ 0.38 s) with margin for the late-entry
/// case (the SOF might land anywhere inside the window).
pub const IDLE_PROBE_BUF_SAMPLES: usize = 2 * AUDIO_RATE as usize;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile2x::ProfileIndex2x;

    /// Reproducible LCG, mirrors the V3 gate test helper.
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Lcg(seed)
        }
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn next_f32(&mut self) -> f32 {
            ((self.next_u32() as f32) / (u32::MAX as f32)) * 2.0 - 1.0
        }
        /// Approx. unit-variance Gaussian via 12-uniform CLT, then /2.
        fn next_gauss(&mut self) -> f32 {
            let s: f32 = (0..12).map(|_| self.next_f32()).sum();
            s / 2.0
        }
    }

    fn noise_buffer(n: usize, rms: f32, seed: u64) -> Vec<f32> {
        let mut rng = Lcg::new(seed);
        let mut buf: Vec<f32> = (0..n).map(|_| rng.next_gauss()).collect();
        let actual_rms = (buf.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
            / n as f64)
            .sqrt() as f32;
        let scale = rms / actual_rms.max(1e-12);
        for x in &mut buf {
            *x *= scale;
        }
        buf
    }

    /// Build a buffer containing the SOF template for `profile`'s
    /// (sps, β) placed at `offset_samples`, with white noise filling
    /// the rest. Caller controls relative amplitudes ⇒ owns the SNR.
    fn buffer_with_sof(
        n: usize,
        profile: ProfileIndex2x,
        offset_samples: usize,
        signal_amp: f32,
        noise_rms: f32,
        seed: u64,
    ) -> Vec<f32> {
        let cfg = profile.to_config();
        let sps = (AUDIO_RATE as f64 / cfg.base.symbol_rate).round() as usize;
        let sof_syms: Vec<Complex64> = sof_for_family(cfg.family).to_vec();
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        let template = modulator::modulate(&sof_syms, sps, sps, &taps, DATA_CENTER_HZ);

        let mut out = noise_buffer(n, noise_rms, seed);
        let template_peak = template
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0f32, f32::max)
            .max(1e-12);
        let scale = signal_amp / template_peak;
        for (i, &t) in template.iter().enumerate() {
            let dst = offset_samples + i;
            if dst < n {
                out[dst] += t * scale;
            }
        }
        out
    }

    #[test]
    fn pure_noise_rejected() {
        let probe = PreambleProbe2x::new(IDLE_PROBE_BUF_SAMPLES);
        // -40 dBFS noise — same level as the V3 perf bench.
        let buf = noise_buffer(IDLE_PROBE_BUF_SAMPLES, 0.01, 0x1234_5678);
        let r = probe.check(&buf);
        assert!(
            r.max_ratio < PROBE_THRESHOLD_2X,
            "noise ratio {:.1} should be < threshold {} (best={:?})",
            r.max_ratio,
            PROBE_THRESHOLD_2X,
            r.best_anchor
        );
    }

    #[test]
    fn high_snr_sof_detected_per_profile() {
        let probe = PreambleProbe2x::new(IDLE_PROBE_BUF_SAMPLES);
        for profile in ProfileIndex2x::ALL {
            let buf = buffer_with_sof(
                IDLE_PROBE_BUF_SAMPLES,
                profile,
                5000,
                0.5,
                0.0001,
                0xCAFE_F00D,
            );
            let r = probe.check(&buf);
            assert!(
                r.passes(PROBE_THRESHOLD_2X),
                "profile {:?}: ratio {:.0} should clear threshold {} (anchor={:?}, ratios={:?})",
                profile,
                r.max_ratio,
                PROBE_THRESHOLD_2X,
                r.best_anchor,
                r.per_template_ratio,
            );
        }
    }

    #[test]
    fn anchor_classification_matches_rs_family() {
        // The probe classifies a clean SOF into the correct (sps, β)
        // family: 500 Bd → Ultra, 1000 Bd → Robust, 1500 Bd → Normal
        // (PLS payload refines NORMAL → HIGH/HIGH+/... downstream).
        let probe = PreambleProbe2x::new(IDLE_PROBE_BUF_SAMPLES);
        for (profile, expected) in [
            (ProfileIndex2x::Ultra2x, ProfileIndex2x::Ultra2x),
            (ProfileIndex2x::Robust2x, ProfileIndex2x::Robust2x),
            (ProfileIndex2x::Normal2x, ProfileIndex2x::Normal2x),
            (ProfileIndex2x::High2x, ProfileIndex2x::Normal2x),
            (ProfileIndex2x::HighPlus2x, ProfileIndex2x::Normal2x),
            (ProfileIndex2x::HighPlusPlus2x, ProfileIndex2x::Normal2x),
            (ProfileIndex2x::HighFiveSix2x, ProfileIndex2x::Normal2x),
            (ProfileIndex2x::HighPlusFiveSix2x, ProfileIndex2x::Normal2x),
        ] {
            let buf = buffer_with_sof(
                IDLE_PROBE_BUF_SAMPLES,
                profile,
                5000,
                0.5,
                0.0001,
                0xCAFE,
            );
            let r = probe.check(&buf);
            assert_eq!(
                r.best_anchor, expected,
                "profile {:?}: expected anchor {:?}, got {:?}, ratios={:?}",
                profile, expected, r.best_anchor, r.per_template_ratio,
            );
        }
    }

    #[test]
    fn cached_probe_returns_same_instance() {
        let p1 = PreambleProbe2x::for_buf_len(IDLE_PROBE_BUF_SAMPLES);
        let p2 = PreambleProbe2x::for_buf_len(IDLE_PROBE_BUF_SAMPLES);
        assert!(std::ptr::eq(p1, p2));
        assert_eq!(p1.anchors().len(), PROBE_TEMPLATES_2X.len());
    }

    #[test]
    fn snr_sweep_monotonic_and_robust() {
        // Sweep noise level upward against a fixed-amplitude SOF.
        // Properties verified:
        //  1. Probe ratio is monotone non-increasing as noise rises
        //     (slack 1.5× for RNG seed).
        //  2. At lowest noise, ratio ≫ threshold (regression check).
        let probe = PreambleProbe2x::new(IDLE_PROBE_BUF_SAMPLES);
        let signal_amp = 0.3_f32;
        let mut last_ratio = f64::INFINITY;
        for &noise_rms in &[0.001_f32, 0.01, 0.03, 0.1, 0.3, 1.0, 3.0] {
            let buf = buffer_with_sof(
                IDLE_PROBE_BUF_SAMPLES,
                ProfileIndex2x::Normal2x,
                12000,
                signal_amp,
                noise_rms,
                0xDEAD,
            );
            let r = probe.check(&buf);
            assert!(
                r.max_ratio <= last_ratio * 1.5,
                "SNR-sweep non-monotone: noise={} ratio={:.0} prev={:.0}",
                noise_rms,
                r.max_ratio,
                last_ratio
            );
            last_ratio = r.max_ratio;
            if noise_rms < 0.005 {
                assert!(
                    r.max_ratio > PROBE_THRESHOLD_2X * 3.0,
                    "clean SOF ratio {:.0} should be ≫ threshold",
                    r.max_ratio
                );
            }
        }
    }

    #[test]
    fn pure_noise_baseline_ratio() {
        // Random Gaussian noise alone should give peak² / mean² ≈
        // 2·ln(fft_len). For our default buf (96000 ingest + 7300
        // max_template_len → fft_len = 131072) that's ≈ 24. Allow 60
        // for random-walk variance but stay safely below threshold.
        let probe = PreambleProbe2x::new(IDLE_PROBE_BUF_SAMPLES);
        let mut max_observed = 0.0_f64;
        for seed in 0u64..16 {
            let buf = noise_buffer(IDLE_PROBE_BUF_SAMPLES, 0.05, seed.wrapping_mul(0x12345));
            let r = probe.check(&buf);
            if r.max_ratio > max_observed {
                max_observed = r.max_ratio;
            }
        }
        assert!(
            max_observed < PROBE_THRESHOLD_2X * 0.7,
            "noise baseline {:.1} too close to threshold {} — increase margin",
            max_observed,
            PROBE_THRESHOLD_2X,
        );
    }

    #[test]
    fn template_count_matches_profile_pitch_buckets() {
        // The probe carries exactly three templates: one per (sps, β)
        // bucket. Every ProfileIndex2x entry must map onto one of these
        // buckets — otherwise the worker would have profiles the gate
        // doesn't know how to pre-detect.
        let probe = PreambleProbe2x::new(IDLE_PROBE_BUF_SAMPLES);
        assert_eq!(probe.anchors().len(), 3);
        for profile in ProfileIndex2x::ALL {
            let cfg = profile.to_config();
            let sps = (AUDIO_RATE as f64 / cfg.base.symbol_rate).round() as usize;
            let beta = cfg.base.beta;
            let matched = PROBE_TEMPLATES_2X
                .iter()
                .any(|&(t_sps, t_beta, _)| t_sps == sps && (t_beta - beta).abs() < 1e-9);
            assert!(
                matched,
                "{profile:?} (sps={sps}, β={beta}) not covered by PROBE_TEMPLATES_2X"
            );
        }
    }
}
