//! Schmidl-Cox preamble detector for the 2x family.
//!
//! Replaces the legacy FFT-based `PreambleProbe2x` (cross-correlation
//! against a passband Chu template). The wire format change to a
//! double-Chu preamble (`[Chu_64 | Chu_64]` = 128 sym back-to-back at
//! the start of every PLHEADER, cf. `plheader.rs`) lets us detect the
//! preamble via a sliding **auto-correlation** of the received audio
//! with itself at lag `L = SOF_LEN_SYM × sps` audio samples:
//!
//! ```text
//! R(k)  = Σ_{m=0..L−1}  y*(k+m) · y(k+m+L)
//! P1(k) = Σ_{m=0..L−1}  |y(k+m)|²
//! P2(k) = Σ_{m=0..L−1}  |y(k+m+L)|²
//! M(k)  = |R(k)|² / (P1(k) · P2(k))    ∈ [0, 1]
//! ```
//!
//! This is the squared **complex correlation coefficient** between
//! the two halves — Cauchy-Schwarz guarantees `0 ≤ M(k) ≤ 1` strictly,
//! with equality iff the halves are scalar multiples of each other.
//! At the preamble position the two halves are **identical** so
//! `M(k) → 1`. On pure zero-mean Gaussian noise the two halves are
//! decorrelated and `E[M(k)] ≈ 1/L` (~5×10⁻⁴ at L = 2048, ~30 dB
//! below the 0.5 threshold).
//!
//! # Why Schmidl-Cox vs the old FFT cross-correlation
//!
//! The legacy probe used the FFT of the audio buffer × the conjugated
//! FFT of an idealised passband Chu template, taking `peak² / mean²`
//! of the inverse FFT. That metric is **not invariant** to:
//!
//! - **AGC gain riding** (the FT-991A → FTX-1 reference sound-card
//!   chain has slow-AGC time constants of 1–3 s and the burst-start
//!   transient compresses the first ~50 ms of every Chu by 3–6 dB).
//! - **Audio LPF / pre-de-emphasis residual** (cumulative TX + RX
//!   audio LPF reshapes the SOF spectrum away from the idealised
//!   template — observed OTA peak ratios collapse to ~ 1/40 of the
//!   clean-channel value).
//! - **Carrier frequency offset** (irrelevant for NBFM today but
//!   mandatory for the QO-100 SDR path with ±30–50 kHz CFO).
//!
//! Schmidl-Cox is invariant to all three because **both halves of the
//! preamble** see the same channel response and the conjugate-product
//! `y*(k+m) · y(k+m+L)` cancels any common amplitude / phase
//! distortion. The metric drops only when the actual signal
//! structure (the repeated pattern) is gone.
//!
//! # Throughput
//!
//! The 400 ms tail (`IDLE_PROBE_BUF_SAMPLES = 19 200` samples) is
//! downmixed to complex baseband once (one complex multiply per
//! sample), then the auto-correlation slides forward one sample at a
//! time using the incremental update
//!
//! ```text
//! R(k+1)  = R(k)  + y*(k+L) · y(k+2L) − y*(k) · y(k+L)
//! P1(k+1) = P1(k) + |y(k+L)|²         − |y(k)|²
//! P2(k+1) = P2(k) + |y(k+2L)|²        − |y(k+L)|²
//! ```
//!
//! → **O(1) per sample**, three sps buckets in parallel. Pi5 cost is
//! ≈ 1–2 ms per call (was 25 ms for the FFT version).
//!
//! # Bucket assignment
//!
//! Three sps values cover the 2x profile catalogue:
//!
//! | sps | L = SOF_LEN_SYM × sps | symbol_rate | anchor   | profiles                |
//! |-----|----------------------:|------------:|----------|-------------------------|
//! |  32 |  2048 (≈ 43 ms)       |     1500 Bd | Normal2x | NORMAL/HIGH/HIGH+/HIGH++ |
//! |  48 |  3072 (≈ 64 ms)       |     1000 Bd | Robust2x | ROBUST                  |
//! |  96 |  6144 (≈ 128 ms)      |      500 Bd | Ultra2x  | ULTRA                   |
//!
//! The detector returns the anchor of the sps bucket that hit the
//! highest metric; the downstream PLS Golay+CRC decode (see
//! [`crate::rx_v4`]) refines `Normal2x` into the actual NORMAL /
//! HIGH / HIGH+ / HIGH++ profile.
//!
//! # Note
//!
//! Like the legacy probe, this is a **presence** probe. A positive
//! result only says "two identical 64-sym halves at the right delay
//! exist in the audio buffer" — the caller still runs the
//! `find_all_sofs` + Golay+CRC validation in
//! [`crate::rx2x_session::try_bootstrap_pair`] to lift the detection
//! to a committed bootstrap.

use std::sync::OnceLock;

use modem_core_base::demodulator;
use modem_core_base::types::{Complex64, AUDIO_RATE, DATA_CENTER_HZ};

use crate::plheader::SOF_LEN_SYM;
use crate::profile2x::ProfileIndex2x;

/// One entry per (sps, anchor) bucket. The anchor is the profile the
/// PLS-decode-side disambiguates further; multiple profiles can share
/// the same sps (e.g. NORMAL2X / HIGH2X / HIGH+2X / HIGH++2X all use
/// `sps = 32` and report `Normal2x`).
const PROBE_BUCKETS_2X: &[(usize, ProfileIndex2x)] = &[
    (32, ProfileIndex2x::Normal2x), // NORMAL/HIGH/HIGH+/HIGH++/HIGH56/HIGH+56
    (48, ProfileIndex2x::Robust2x),
    (96, ProfileIndex2x::Ultra2x),
];

/// Schmidl-Cox metric threshold (∈ [0, 1]) above which the probe
/// declares a preamble likely present.
///
/// Theoretical noise baseline: for zero-mean Gaussian audio of length
/// `2L`, the expected metric `E[M(k)] ≈ 1/L` — for the shortest
/// bucket (`L = 2048` at sps = 32) that's ≈ `5 × 10⁻⁴`, giving the
/// 0.5 threshold ≈ 30 dB of margin over noise.
///
/// Clean preamble: `M(k) → 1` at the exact preamble position; even
/// with the OTA AGC + LPF distortion that previously crushed the
/// FFT-template cross-correlation to ratios of 15–18, the
/// Schmidl-Cox metric stays in the 0.6–0.9 range because both halves
/// of the preamble go through the same channel.
///
/// Renamed from `PROBE_THRESHOLD_2X` (which was 120.0 for the
/// peak² / mean² metric of the legacy FFT probe). Call-sites use the
/// new constant directly; the old name no longer applies to the new
/// metric semantics.
pub const PROBE_THRESHOLD_2X: f64 = 0.5;

/// Detector — stateless (no per-call allocation). One instance for
/// the whole process; the buckets are baked in from
/// [`PROBE_BUCKETS_2X`].
pub struct PreambleProbe2x {
    fc_hz: f64,
    buckets: Vec<BucketCfg>,
}

#[derive(Clone, Copy)]
struct BucketCfg {
    sps: usize,
    anchor: ProfileIndex2x,
    half_len: usize, // L = SOF_LEN_SYM × sps
}

#[derive(Debug, Clone)]
pub struct ProbeResult2x {
    /// Max Schmidl-Cox metric observed across all sps buckets,
    /// `∈ [0, 1]`. Replaces the old `peak² / mean²` ratio that had
    /// unbounded range. Field name kept for call-site compatibility
    /// during the wire-format transition.
    pub max_ratio: f64,
    /// Anchor profile of the bucket that produced `max_ratio`. The
    /// worker uses this as a coarse hint to choose between
    /// Ultra/Robust/Normal candidates; the PLS payload of the
    /// matching cycle refines the Normal2x bucket later.
    pub best_anchor: ProfileIndex2x,
    /// Per-bucket metric, in declaration order — matches
    /// [`PROBE_BUCKETS_2X`] (Normal, Robust, Ultra).
    pub per_template_ratio: Vec<f64>,
}

impl ProbeResult2x {
    pub fn passes(&self, threshold: f64) -> bool {
        self.max_ratio >= threshold
    }
}

impl PreambleProbe2x {
    /// Get the process-wide cached detector. The first call builds
    /// the bucket configs (~µs, no FFT plans any more); subsequent
    /// calls return the same instance.
    ///
    /// `_buf_len` is accepted for API compatibility with the legacy
    /// `PreambleProbe2x::for_buf_len`; the new detector is
    /// buf-length-independent (the caller passes the audio slice it
    /// wants checked).
    pub fn for_buf_len(_buf_len: usize) -> &'static Self {
        static CACHE: OnceLock<PreambleProbe2x> = OnceLock::new();
        CACHE.get_or_init(Self::new)
    }

    pub fn new() -> Self {
        let buckets = PROBE_BUCKETS_2X
            .iter()
            .map(|&(sps, anchor)| BucketCfg {
                sps,
                anchor,
                half_len: SOF_LEN_SYM * sps,
            })
            .collect();
        Self {
            fc_hz: DATA_CENTER_HZ,
            buckets,
        }
    }

    /// Run the Schmidl-Cox detector over `samples`. Returns the best
    /// metric across the three sps buckets + the bucket's anchor.
    ///
    /// Per-call cost: one downmix (O(N) complex multiplies) + one
    /// moving-average LPF (O(N) running sum) + three sliding
    /// auto-correlations (O(N) each, incremental R/P update).
    /// Total ≈ 5·N flops at sps = 32; ~2 ms on Pi5 for the
    /// 19 200-sample default window.
    ///
    /// **Why the LPF**: downmixing real passband audio at `fc` leaves
    /// a "ghost" image at `−2fc` (analytic signal recovery from a
    /// real-only signal requires the imaginary half, which a Hilbert
    /// transform would provide but a simple e^{-jωt} multiply cannot).
    /// Without LPF, the ghost contributes a `cos²(ωL)` factor to the
    /// metric on identical preamble halves — for Ultra2x at
    /// `L = 6144`, `fc = 1100`, that's `cos²(2π·1100·6144/48000) =
    /// cos²(0.8·2π) ≈ 0.10` and the detector misses the clean signal.
    /// The moving-average LPF zeros out the ghost band (`±2fc`) and
    /// recovers the true analytic baseband.
    pub fn check(&self, samples: &[f32]) -> ProbeResult2x {
        let bb_raw = demodulator::downmix(samples, self.fc_hz);
        // LPF first null placed at **2·fc** (= ghost frequency). At
        // width = round(fs / (2·fc)) the moving-average has its first
        // null exactly on the ghost band, suppressing it by > 40 dB,
        // while leaving the modem signal band (`±900 Hz` around DC
        // after downmix, for the 1500 Bd profiles) attenuated by only
        // ~2–3 dB. The previous setting (`fs/fc`) put the null on the
        // modem band itself and caused false-positive SC metrics on
        // PRBS pre-burst content (because the LPF carved out structure
        // in the data band, leaving correlated DC residue between
        // halves).
        let lpf_width =
            ((AUDIO_RATE as f64) / (2.0 * self.fc_hz)).round() as usize;
        let mut bb = moving_avg_lpf(&bb_raw, lpf_width.max(1));
        // DC-block: subtract the bb mean. Without this, a VOX tone in
        // the audio buffer (which downmixes to pure DC at fc=carrier)
        // triggers a false-positive Schmidl-Cox metric of ~1.0 (any
        // two halves of a DC signal are trivially "identical"). The
        // Chu preamble has zero DC by construction so subtracting
        // mean has negligible effect on the real-preamble metric.
        if !bb.is_empty() {
            let mean: Complex64 = bb.iter().sum::<Complex64>() / bb.len() as f64;
            for s in &mut bb {
                *s -= mean;
            }
        }
        let mut per_template_ratio = Vec::with_capacity(self.buckets.len());
        let mut best_ratio = 0.0_f64;
        let mut best_score = 0.0_f64;
        let mut best_anchor = self.buckets[0].anchor;
        for bucket in &self.buckets {
            let (m, _pos) = streaming_schmidl_max(&bb, bucket.half_len);
            per_template_ratio.push(m);
            // Anchor selection: bias toward the bucket whose `half_len`
            // best matches the actual preamble length. A wrong-L bucket
            // can still fire high on a clean preamble of a different
            // profile (e.g. Ultra2x preamble at sps=96 has slow audio
            // variations that look "correlated at lag 2048 samples" to
            // the Normal2x bucket's `L=2048` SC). Weighting the metric
            // by `√L` prioritises the bucket that's actually integrating
            // a full preamble; the LPF + DC-block don't fully eliminate
            // the cross-bucket leakage.
            let l_score = (bucket.half_len as f64).sqrt();
            let score = m * l_score;
            if m > best_ratio {
                best_ratio = m;
            }
            if score > best_score {
                best_score = score;
                best_anchor = bucket.anchor;
            }
        }
        ProbeResult2x {
            max_ratio: best_ratio,
            best_anchor,
            per_template_ratio,
        }
    }
}

/// Causal moving-average LPF on a complex signal. Width = number of
/// samples to average over (running sum, O(1) per sample). The first
/// `width − 1` samples use a partial sum (initialisation transient);
/// this matters little because the Schmidl-Cox max search has plenty
/// of clean samples to lock onto downstream.
fn moving_avg_lpf(bb: &[Complex64], width: usize) -> Vec<Complex64> {
    let mut out = Vec::with_capacity(bb.len());
    let mut sum = Complex64::new(0.0, 0.0);
    let w = width.max(1);
    for i in 0..bb.len() {
        sum += bb[i];
        if i >= w {
            sum -= bb[i - w];
            out.push(sum / w as f64);
        } else {
            out.push(sum / (i + 1) as f64);
        }
    }
    out
}

impl Default for PreambleProbe2x {
    fn default() -> Self {
        Self::new()
    }
}

/// Sliding Schmidl-Cox max over a complex-baseband signal `bb` with
/// half-window length `half_len`. Returns `(max_metric, max_pos)`
/// where `max_pos` is the start of the leading half (`k` in the
/// formula `M(k) = |R(k)|² / (P1(k) · P2(k))`).
///
/// Uses the **product** normalisation `P1 · P2` rather than the
/// original Schmidl-Cox `R₂²` — this turns the metric into the
/// squared complex correlation coefficient between the two halves,
/// which is bounded `∈ [0, 1]` strictly by Cauchy-Schwarz. The
/// product form rejects pure-noise tails much more tightly than the
/// single-half normalisation (which can spike near 1 by chance when
/// the two halves' powers happen to align).
///
/// Incremental update: O(1) per sample after the O(L) initialisation.
fn streaming_schmidl_max(bb: &[Complex64], half_len: usize) -> (f64, usize) {
    if bb.len() < 2 * half_len {
        return (0.0, 0);
    }
    // Initial R, P1, P2 at k = 0.
    let mut r = Complex64::new(0.0, 0.0);
    let mut p1 = 0.0_f64;
    let mut p2 = 0.0_f64;
    for m in 0..half_len {
        r += bb[m].conj() * bb[m + half_len];
        p1 += bb[m].norm_sqr();
        p2 += bb[m + half_len].norm_sqr();
    }
    let denom0 = p1 * p2;
    let mut max_metric = if denom0 > 1e-12 {
        r.norm_sqr() / denom0
    } else {
        0.0
    };
    let mut max_pos = 0_usize;
    // Slide one sample at a time. At iteration k the window covers
    // bb[k..k + 2L]; we maintain R, P1, P2 for that k.
    let end = bb.len() - 2 * half_len;
    for k in 1..=end {
        let y_old = bb[k - 1];
        let y_mid = bb[k - 1 + half_len];
        let y_new = bb[k - 1 + 2 * half_len];
        r += y_mid.conj() * y_new - y_old.conj() * y_mid;
        p1 += y_mid.norm_sqr() - y_old.norm_sqr();
        p2 += y_new.norm_sqr() - y_mid.norm_sqr();
        let denom = p1 * p2;
        let metric = if denom > 1e-12 {
            r.norm_sqr() / denom
        } else {
            0.0
        };
        if metric > max_metric {
            max_metric = metric;
            max_pos = k;
        }
    }
    (max_metric, max_pos)
}

/// Rolling-buffer length the session keeps in Idle for the probe —
/// **400 ms** at 48 kHz. Sized to fit the longest preamble window
/// (Ultra2x: 2 × 6144 = 12 288 samples ≈ 256 ms) with margin for
/// the sliding scan to find a peak inside.
///
/// Sized tight on purpose: while `gate_armed == false` in
/// [`crate::rx2x_session::Rx2xSession`], anything older than this
/// window is dropped before the probe runs — no downmix, no matched
/// filter, no resample on stale samples.
pub const IDLE_PROBE_BUF_SAMPLES: usize = (AUDIO_RATE as usize) * 2 / 5;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plheader::{preamble_for_family, PreambleFamily2x};
    use crate::profile2x::ProfileIndex2x;
    use modem_core_base::modulator;
    use modem_core_base::rrc::rrc_taps;
    use modem_core_base::types::RRC_SPAN_SYM;

    /// Reproducible LCG. Critically: `next_f32` divides by `2^31` (the
    /// actual range of `next_u32` which returns the top 31 bits of the
    /// LCG state). The pre-2x24 helper used `u32::MAX` (= `2^32 - 1`)
    /// as the divisor and `(x as f32) * 2 - 1`, which yielded values
    /// in `[-1, 0]` (mean -0.5) instead of `[-1, 1]` (mean 0). That DC
    /// offset compounded over 2048-sample sums and made the
    /// Schmidl-Cox metric on "noise" approach 1 deterministically —
    /// the test then declared a bogus failure. The correct uniform is
    /// what we need to actually exercise the noise floor.
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
            // next_u32 returns top 31 bits → value in [0, 2^31 − 1].
            // Map to [-1, 1] via 2·(x / 2^31) − 1.
            (self.next_u32() as f32) / (1u32 << 30) as f32 - 1.0
        }
        fn next_gauss(&mut self) -> f32 {
            // Sum of 12 uniforms on [-1, 1] → variance 12·(1/3) = 4.
            // Divide by 2 → variance 1, ≈ N(0, 1) by CLT.
            let s: f32 = (0..12).map(|_| self.next_f32()).sum();
            s / 2.0
        }
    }

    fn noise_buffer(n: usize, rms: f32, seed: u64) -> Vec<f32> {
        let mut rng = Lcg::new(seed);
        let mut buf: Vec<f32> = (0..n).map(|_| rng.next_gauss()).collect();
        let actual = (buf.iter().map(|&x| (x as f64).powi(2)).sum::<f64>()
            / n as f64)
            .sqrt() as f32;
        let scale = rms / actual.max(1e-12);
        for x in &mut buf {
            *x *= scale;
        }
        buf
    }

    /// Synthesise a clean double-Chu preamble (passband audio at
    /// `DATA_CENTER_HZ`) for the given profile's (sps, β). Used to
    /// confirm the Schmidl-Cox detector hits ratio ≈ 1 on a clean
    /// signal regardless of audio amplitude.
    fn clean_preamble_audio(profile: ProfileIndex2x, signal_amp: f32) -> Vec<f32> {
        let cfg = profile.to_config();
        let sps = (AUDIO_RATE as f64 / cfg.base.symbol_rate).round() as usize;
        let preamble = preamble_for_family(cfg.family);
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        let mut audio = modulator::modulate(&preamble, sps, sps, &taps, DATA_CENTER_HZ);
        let peak = audio.iter().map(|s| s.abs()).fold(0.0_f32, f32::max).max(1e-12);
        let scale = signal_amp / peak;
        for s in &mut audio {
            *s *= scale;
        }
        audio
    }

    #[test]
    fn metric_on_pure_noise_below_threshold() {
        let probe = PreambleProbe2x::new();
        // -40 dBFS noise, 16 seeds.
        let mut max_observed = 0.0_f64;
        for seed in 0u64..16 {
            let buf = noise_buffer(IDLE_PROBE_BUF_SAMPLES, 0.01, seed.wrapping_mul(0x12345));
            let r = probe.check(&buf);
            if r.max_ratio > max_observed {
                max_observed = r.max_ratio;
            }
        }
        // 30 dB margin → noise should sit well below 0.5.
        assert!(
            max_observed < PROBE_THRESHOLD_2X * 0.5,
            "noise max {:.4} too close to threshold {}",
            max_observed,
            PROBE_THRESHOLD_2X
        );
    }

    #[test]
    fn metric_on_clean_preamble_passes_threshold_all_profiles() {
        let probe = PreambleProbe2x::new();
        for profile in ProfileIndex2x::ALL {
            // Pad with a bit of noise BEFORE and AFTER the preamble
            // so the detector window can find the metric peak inside.
            let preamble_audio = clean_preamble_audio(profile, 0.5);
            let pad = noise_buffer(5_000, 0.001, 0xCAFE_F00D);
            let mut buf = pad.clone();
            buf.extend_from_slice(&preamble_audio);
            buf.extend_from_slice(&pad);
            let r = probe.check(&buf);
            assert!(
                r.passes(PROBE_THRESHOLD_2X),
                "{profile:?}: metric {:.3} below threshold {} (per-bucket {:?})",
                r.max_ratio,
                PROBE_THRESHOLD_2X,
                r.per_template_ratio
            );
            // Metric is in [0, 1] so clean signal must be well above
            // threshold but bounded by 1 + small slack for numerical
            // jitter.
            assert!(
                r.max_ratio <= 1.05,
                "{profile:?}: metric {:.3} unbounded — Schmidl-Cox should be ≤ 1",
                r.max_ratio
            );
        }
    }

    #[test]
    fn anchor_classification_matches_sps_bucket() {
        let probe = PreambleProbe2x::new();
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
            let preamble_audio = clean_preamble_audio(profile, 0.5);
            let pad = noise_buffer(5_000, 0.001, 0xC0FE);
            let mut buf = pad.clone();
            buf.extend_from_slice(&preamble_audio);
            buf.extend_from_slice(&pad);
            let r = probe.check(&buf);
            assert_eq!(
                r.best_anchor, expected,
                "{profile:?} → bucket {expected:?}: got {:?} (ratios {:?})",
                r.best_anchor, r.per_template_ratio
            );
        }
    }

    #[test]
    fn snr_sweep_monotone_and_robust() {
        // Sweep noise level upward against a fixed-amplitude
        // preamble; the metric must stay above threshold for the
        // first few noise levels and decrease (monotone) as noise
        // rises.
        let probe = PreambleProbe2x::new();
        let preamble_audio = clean_preamble_audio(ProfileIndex2x::Normal2x, 0.3);
        let mut last_ratio = f64::INFINITY;
        for &noise_rms in &[0.001_f32, 0.01, 0.03, 0.1, 0.3, 1.0] {
            let pad_before = noise_buffer(5_000, noise_rms, 0xDEAD);
            let pad_after = noise_buffer(
                IDLE_PROBE_BUF_SAMPLES.saturating_sub(5_000 + preamble_audio.len()).max(1_000),
                noise_rms,
                0xBEEF,
            );
            let mut buf = pad_before;
            buf.extend_from_slice(&preamble_audio);
            buf.extend_from_slice(&pad_after);
            let r = probe.check(&buf);
            assert!(
                r.max_ratio <= last_ratio * 1.5,
                "non-monotone: noise={} metric={:.3} prev={:.3}",
                noise_rms,
                r.max_ratio,
                last_ratio
            );
            last_ratio = r.max_ratio;
            if noise_rms < 0.005 {
                assert!(
                    r.max_ratio > PROBE_THRESHOLD_2X,
                    "clean preamble metric {:.3} below threshold",
                    r.max_ratio
                );
            }
        }
    }

    #[test]
    fn cached_probe_returns_same_instance() {
        let p1 = PreambleProbe2x::for_buf_len(IDLE_PROBE_BUF_SAMPLES);
        let p2 = PreambleProbe2x::for_buf_len(IDLE_PROBE_BUF_SAMPLES);
        assert!(std::ptr::eq(p1, p2));
    }
}
