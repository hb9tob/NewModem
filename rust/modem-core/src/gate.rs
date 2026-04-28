//! Cheap preamble-presence probe used to gate the expensive `rx_v3_after`
//! pipeline at idle.
//!
//! Motivation : on Surface Pro 7 (i5-1035G4) each idle scan tick was costing
//! ~1000 ms — 6 successive matched-filter passes (5 in `detect_best_profile`
//! + 1 in `rx_v3_after`) at ~96 k samples × ~384 RRC taps = ~37 M complex
//! mults each. The worker saturated SCAN_INTERVAL_MS continuously, leaving
//! the audio capture starved.
//!
//! The matched filter cost is unavoidable on a real preamble (the data
//! decode needs that filtered signal anyway) but it's wasted on pure noise
//! or band-noise with no preamble structure. An RMS-energy gate is unsafe
//! here because NBFM band noise (squelch open) can have very high energy
//! without any signal content. We need a **structure-aware** gate :
//! something that fires on the specific spectral signature of the 256-symbol
//! QPSK preamble + RRC pulse + carrier at `DATA_CENTER_HZ`.
//!
//! Implementation : pre-compute the **conjugate FFT of the passband
//! preamble template** for each unique `(sps, pitch, beta)` tuple, once at
//! probe construction. At each call : one rFFT of the audio buffer
//! (buffer is real f32 → use realfft for ~2× speedup vs full-complex FFT) ;
//! per template, multiply spectra and irfft → linear cross-correlation in
//! the time domain. The peak/mean ratio of `|y|²` is a SNR-like discriminant
//! :
//!   - Pure Gaussian noise of length L : peak²/mean² ≈ 2·ln(L), so for our
//!     L = 131072 (next pow2 above buf_len + max_template_len), the noise
//!     baseline is ≈ 25.
//!   - Clean V3 preamble : the cross-correlation at the alignment offset
//!     is the squared correlation of 256 unit-circle symbols → peak²/mean²
//!     in the thousands.
//!
//! The default `PROBE_THRESHOLD = 100` sits > 6 dB above the noise baseline
//! and > 6 dB below typical clean-preamble peaks. Calibrate further on
//! real captures before relying on it for marginal-SNR work.
//!
//! Cost on SP7 : one FFT(L) + 4 × (multiply + iFFT(L) + magnitude scan) ≈
//! 25–30 ms per call, vs ~720 ms for the equivalent matched-filter chain.
//! ~25× speedup ; the saved cycles are reclaimed by the audio capture
//! thread.
//!
//! Note : the probe is **not** a decoder. A positive result only says
//! "something looks like a preamble" — the caller still runs the full
//! `detect_best_profile` + `rx_v3_after` pipeline to extract the actual
//! profile, header, and payload.

use std::sync::Arc;
use std::sync::OnceLock;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::modulator;
use crate::preamble::{self, PreambleFamily};
use crate::profile::ProfileIndex;
use crate::rrc::rrc_taps;
use crate::types::{AUDIO_RATE, DATA_CENTER_HZ, N_PREAMBLE, RRC_SPAN_SYM};

/// One pre-computed FFT(template) per anchor profile. The receiver runs a
/// single rFFT(audio) and one spectral multiply + iFFT per template to
/// classify the on-air signal's anchor profile — replacing the 5-profile
/// sweep through `detect_best_profile` (~720 ms) with one ~25 ms call.
///
/// Each entry : `(family, sps, pitch, β, anchor)`. Family A splits into
/// **two** templates because its profiles disagree on pitch :
///   - NORMAL/HIGH/HIGH+ partagent `(sps=32, pitch=32, β=0.20)` — Nyquist τ=1.0,
///     header refine ensuite Normal → High → HighPlus.
/// Without that split, find_all_preambles downstream looks for pulses at
/// the wrong spacing for whichever sub-profile didn't get the anchor, and
/// the drift over 256 symbols (≈ 512 samples) destroys the correlation.
///
/// MEGA (FTN τ=30/32, pitch=30) basculé expérimental 2026-04-28 → retiré
/// du gate ; reste accessible en mode forcé RX.
const PROBE_TEMPLATES: &[(PreambleFamily, usize, usize, f64, ProfileIndex)] = &[
    (PreambleFamily::A, 32, 32, 0.20, ProfileIndex::Normal), // NORMAL/HIGH/HIGH+ (header refines)
    (PreambleFamily::B, 48, 48, 0.25, ProfileIndex::Robust),
    (PreambleFamily::C, 96, 96, 0.25, ProfileIndex::Ultra),
];

/// Default peak²/mean² ratio above which the probe declares a preamble
/// likely present. See module-level docs for the calibration rationale.
pub const PROBE_THRESHOLD: f64 = 100.0;

/// Pre-built FFT plan + per-template conjugated spectra. Constructed once
/// per buffer length via `for_buf_len()` and reused for every scan tick.
pub struct PreambleProbe {
    fft_len: usize,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    /// One conjugated spectrum per `PROBE_TEMPLATES` entry.
    /// Length = `fft_len/2 + 1`.
    templates: Vec<Vec<Complex32>>,
    anchors: Vec<ProfileIndex>,
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// Best peak²/mean² ratio observed across all templates.
    pub max_ratio: f64,
    /// Anchor `ProfileIndex` of the template that produced `max_ratio`.
    /// Downstream code uses this to set the worker's profile directly
    /// (`scan_and_route` in `rx_worker.rs`) ; the protocol header still
    /// refines NORMAL→HIGH within the same pitch later in the pipeline.
    pub best_anchor: ProfileIndex,
    /// All templates' ratios, in declaration order — matches
    /// `PROBE_TEMPLATES` (Normal, Mega, Robust, Ultra at the time of
    /// writing).
    pub per_template_ratio: Vec<f64>,
}

impl ProbeResult {
    /// Convenience : preamble family of `best_anchor`. Useful for log
    /// formatting and backwards-compat callers that grouped by family.
    pub fn best_family(&self) -> PreambleFamily {
        self.best_anchor.preamble_family()
    }
}

impl ProbeResult {
    pub fn passes(&self, threshold: f64) -> bool {
        self.max_ratio >= threshold
    }
}

impl PreambleProbe {
    /// Get a process-wide cached probe sized for `buf_len`. The first call
    /// builds the plan + templates (~5 ms one-shot) ; subsequent calls
    /// return the same instance.
    ///
    /// In practice the worker only ever passes one `buf_len` value
    /// (PREROLL_SECONDS × AUDIO_RATE = 96000 in idle), so the cache fits
    /// in a single `OnceLock`.
    pub fn for_buf_len(buf_len: usize) -> &'static Self {
        static CACHE: OnceLock<PreambleProbe> = OnceLock::new();
        CACHE.get_or_init(|| Self::new(buf_len))
    }

    /// Constructor — builds FFT plans and per-template conjugated spectra.
    /// Public so unit tests can build instances at non-default buffer
    /// lengths ; production code should use `for_buf_len()`.
    pub fn new(buf_len: usize) -> Self {
        // FFT length = next power of 2 above `buf_len + max_template_len`,
        // so the linear (non-circular) cross-correlation fits without
        // wrap-around. ULTRA's template is the longest : 256 syms × sps=96
        // + RRC span × sps = 256·96 + 12·96 = 25728.
        let max_template_len = N_PREAMBLE * 96 + RRC_SPAN_SYM * 96;
        let fft_len = next_pow2(buf_len + max_template_len);

        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(fft_len);
        let inverse = planner.plan_fft_inverse(fft_len);

        let mut templates = Vec::with_capacity(PROBE_TEMPLATES.len());
        let mut anchors = Vec::with_capacity(PROBE_TEMPLATES.len());
        for &(family, sps, pitch, beta, anchor) in PROBE_TEMPLATES {
            let pre_syms = preamble::make_preamble_for(family);
            let taps = rrc_taps(beta, RRC_SPAN_SYM, sps);
            // Reuse the production modulator so the template matches what
            // the TX actually emits, byte-for-byte.
            let template = modulator::modulate(&pre_syms, sps, pitch, &taps, DATA_CENTER_HZ);

            // Energy-normalise the template so cross-template ratios are
            // directly comparable (modulator::modulate peak-normalises,
            // which biases templates with denser pulse overlap toward
            // smaller per-pulse amplitude — over-rewards thinner templates
            // on cross-talk in the FFT correlation).
            let energy: f64 = template.iter().map(|&x| (x as f64) * (x as f64)).sum();
            let scale = if energy > 1e-12 { 1.0 / energy.sqrt() as f32 } else { 1.0 };

            // Zero-pad to fft_len, forward FFT, conjugate (so multiplying
            // audio_fft by this gives the cross-correlation spectrum in
            // a single elementwise step).
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

        PreambleProbe {
            fft_len,
            forward,
            inverse,
            templates,
            anchors,
        }
    }

    pub fn fft_len(&self) -> usize { self.fft_len }
    pub fn anchors(&self) -> &[ProfileIndex] { &self.anchors }

    /// Run the probe against `samples`. Cost per call ≈ 1 forward rFFT
    /// + N_TEMPLATES × (mul + inverse rFFT + |y|² scan).
    ///
    /// `samples.len()` must be ≤ `fft_len()`. If shorter, the audio is
    /// zero-padded to fill the FFT.
    pub fn check(&self, samples: &[f32]) -> ProbeResult {
        // 1. Forward rFFT of audio (zero-padded if needed).
        let mut input = vec![0.0f32; self.fft_len];
        let n = samples.len().min(self.fft_len);
        input[..n].copy_from_slice(&samples[..n]);
        let mut audio_spec = self.forward.make_output_vec();
        self.forward
            .process(&mut input, &mut audio_spec)
            .expect("rFFT forward (audio)");

        // 2. Per-template cross-correlation : multiply spectra → iFFT →
        //    peak²/mean² ratio of the time-domain output.
        let spec_len = audio_spec.len();
        let mut work_spec = vec![Complex32::new(0.0, 0.0); spec_len];
        let mut output = vec![0.0f32; self.fft_len];

        let mut per_template_ratio = Vec::with_capacity(self.templates.len());
        let mut best_ratio = 0.0f64;
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

            let mut max_sqr = 0.0f64;
            let mut sum_sqr = 0.0f64;
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

        ProbeResult {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ProfileIndex;

    /// Reproducible LCG, same RNG used in the perf bench for parity.
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self { Lcg(seed) }
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
        let actual_rms =
            (buf.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / n as f64).sqrt() as f32;
        let scale = rms / actual_rms.max(1e-12);
        for x in &mut buf {
            *x *= scale;
        }
        buf
    }

    /// Build a buffer holding a passband preamble at `offset_samples` from
    /// the start, with white noise filling the rest. Caller controls the
    /// preamble vs noise amplitude (treats the preamble as the "signal").
    fn buffer_with_preamble(
        n: usize,
        profile: ProfileIndex,
        offset_samples: usize,
        signal_amp: f32,
        noise_rms: f32,
        seed: u64,
    ) -> Vec<f32> {
        let cfg = profile.to_config();
        let sps = (AUDIO_RATE as f64 / cfg.symbol_rate) as usize;
        let pitch = (sps as f64 * cfg.tau).round() as usize;
        let pre = preamble::make_preamble_for(profile.preamble_family());
        let taps = rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
        let template =
            modulator::modulate(&pre, sps, pitch, &taps, DATA_CENTER_HZ);

        let mut out = noise_buffer(n, noise_rms, seed);
        // `modulate` peak-normalises to PEAK_NORMALIZE ; rescale to
        // `signal_amp` so the test owns the SNR.
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
        let probe = PreambleProbe::new(96000);
        // -40 dBFS noise — same level as the perf bench.
        let buf = noise_buffer(96000, 0.01, 0x1234_5678);
        let r = probe.check(&buf);
        // Random Gaussian peak/mean ≈ 2·ln(L) ≈ 24 for L = 131072. Should
        // be safely under PROBE_THRESHOLD.
        assert!(
            r.max_ratio < PROBE_THRESHOLD,
            "noise ratio {:.1} should be < threshold {} (best={:?})",
            r.max_ratio, PROBE_THRESHOLD, r.best_anchor
        );
    }

    #[test]
    fn high_snr_preamble_detected_per_profile() {
        let probe = PreambleProbe::new(96000);
        for profile in [
            ProfileIndex::Normal,
            ProfileIndex::High,
            ProfileIndex::HighPlus,
            ProfileIndex::Robust,
            ProfileIndex::Ultra,
        ] {
            let buf = buffer_with_preamble(
                96000, profile, 5000,
                /* signal_amp */ 0.5,
                /* noise_rms  */ 0.0001,
                0xCAFEF00D,
            );
            let r = probe.check(&buf);
            assert!(
                r.passes(PROBE_THRESHOLD),
                "profile {:?} : ratio {:.0} should clear threshold {} (anchor={:?}, ratios={:?})",
                profile, r.max_ratio, PROBE_THRESHOLD, r.best_anchor, r.per_template_ratio,
            );
        }
    }

    #[test]
    fn anchor_classification_matches_profile() {
        // With distinct preamble sequences per family AND a per-pitch
        // template within family A, the gate should classify the on-air
        // signal into the correct anchor for every profile :
        //   - NORMAL/HIGH/HIGH+ share pitch=32 → anchor NORMAL (header refines).
        //   - ROBUST and ULTRA have unique (sps, β) → unambiguous.
        // MEGA (FTN pitch=30) basculé expérimental → plus dans PROBE_TEMPLATES,
        // décodable seulement en mode forcé.
        let probe = PreambleProbe::new(96000);
        for (profile, expected) in [
            (ProfileIndex::Normal,   ProfileIndex::Normal),
            (ProfileIndex::High,     ProfileIndex::Normal), // header → HIGH downstream
            (ProfileIndex::HighPlus, ProfileIndex::Normal), // header → HIGH+ downstream
            (ProfileIndex::Robust,   ProfileIndex::Robust),
            (ProfileIndex::Ultra,    ProfileIndex::Ultra),
        ] {
            let buf = buffer_with_preamble(
                96000, profile, 5000, 0.5, 0.0001, 0xCAFE,
            );
            let r = probe.check(&buf);
            assert_eq!(
                r.best_anchor, expected,
                "profile {:?} : expected anchor {:?}, got {:?}, ratios={:?}",
                profile, expected, r.best_anchor, r.per_template_ratio,
            );
        }
    }

    #[test]
    fn snr_sweep_monotonic_and_robust() {
        // Sweep noise level upward against a fixed-amplitude preamble.
        // Verify two properties :
        //  1. The probe ratio is roughly monotone-decreasing as noise
        //     increases (slack for RNG seed).
        //  2. At the lowest noise, the ratio is ≫ threshold (sanity check
        //     against template-broken regressions).
        // We do NOT assert that the ratio drops below threshold within
        // any specific noise range — the probe being more sensitive than
        // expected is a feature, not a failure mode.
        let probe = PreambleProbe::new(96000);
        let signal_amp = 0.3f32;
        let mut last_ratio = f64::INFINITY;
        for &noise_rms in &[0.001f32, 0.01, 0.03, 0.1, 0.3, 1.0, 3.0] {
            let buf = buffer_with_preamble(
                96000, ProfileIndex::Normal, 12000,
                signal_amp, noise_rms, 0xDEAD,
            );
            let r = probe.check(&buf);
            assert!(
                r.max_ratio <= last_ratio * 1.5,
                "SNR-sweep non-monotone : noise={} ratio={:.0} prev={:.0}",
                noise_rms, r.max_ratio, last_ratio
            );
            last_ratio = r.max_ratio;
            if noise_rms < 0.005 {
                assert!(
                    r.max_ratio > PROBE_THRESHOLD * 5.0,
                    "clean preamble ratio {:.0} should be ≫ threshold",
                    r.max_ratio
                );
            }
        }
    }

    #[test]
    fn pure_noise_baseline_ratio() {
        // Sanity : random Gaussian noise alone should give a peak²/mean²
        // ratio close to the theoretical value 2·ln(fft_len) for L=131072,
        // ≈ 24. Empirically allow up to 60 (random walk variance) but
        // always well below PROBE_THRESHOLD.
        let probe = PreambleProbe::new(96000);
        let mut max_observed = 0.0f64;
        for seed in 0u64..16 {
            let buf = noise_buffer(96000, 0.05, seed.wrapping_mul(0x12345));
            let r = probe.check(&buf);
            if r.max_ratio > max_observed { max_observed = r.max_ratio; }
        }
        assert!(
            max_observed < PROBE_THRESHOLD * 0.6,
            "noise baseline {:.1} too close to threshold {} — increase margin",
            max_observed, PROBE_THRESHOLD,
        );
    }

    #[test]
    fn cached_probe_returns_same_instance() {
        let p1 = PreambleProbe::for_buf_len(96000);
        let p2 = PreambleProbe::for_buf_len(96000);
        assert!(std::ptr::eq(p1, p2));
        assert_eq!(p1.anchors().len(), PROBE_TEMPLATES.len());
    }
}
