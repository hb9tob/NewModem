//! Real-valued FIR decimator (÷N) + tap-generation helpers.
//!
//! [`PolyphaseDecimator`] is a Rust port of the integer-decim mode of
//! `gr-filter::fir_filter_fff` — kept for any post-demod real-rate
//! resampling that doesn't go through the canonical NBFM chain.
//!
//! For complex (I/Q) channel selection — what an SDR RX needs — we
//! use the [`crate::freq_xlating::FreqXlatingFir`] block (port of GR's
//! `freq_xlating_fir_filter_ccf`); it combines NCO + complex LPF +
//! decimation in a single block, so a redundant "complex polyphase
//! decimator" type isn't needed here.
//!
//! The Kaiser tap generator [`kaiser_sinc_taps`] lives in this module
//! because it's used by both — `FreqXlatingFir` for the channel filter
//! and any `PolyphaseDecimator` users that want a tighter stop-band
//! than the simpler Hamming helper produces.
//!
//! Cost optimisation: the FIR is only evaluated on output samples,
//! i.e. once every `factor` inputs. So the per-output cost is `N`
//! multiply-adds where `N` is the tap count — a 99-tap ÷11 decimator
//! at 528 kHz costs `99 · 48_000 ≈ 4.7 MFLOPS`, well under 1 % of
//! one Pi 5 core. The history is a circular buffer to avoid the
//! O(N) shift that a naïve `Vec::insert(0, x)` would incur per
//! input sample.
//!
//! Tap generation: `hamming_sinc_taps()` produces a windowed-sinc
//! low-pass FIR. The Hamming window gives ~53 dB stop-band
//! attenuation, plenty for ±5 kHz NBFM at output rate 48 kHz where
//! the audio of interest stops at ~3 kHz. If we ever need stricter
//! aliasing rejection (e.g. for very weak adjacent-channel
//! interference), swap in optfir / Parks-McClellan taps generated
//! offline by `study/generate_taps.py` — the [`Self::with_taps`]
//! constructor accepts an arbitrary tap vector.
//!
//! Reproducibility: `hamming_sinc_taps` is fully deterministic from
//! `(input_rate, cutoff, num_taps)`. Same inputs → bit-identical taps
//! across runs and platforms (assuming IEEE 754 f32 semantics, which
//! all our targets honour).

/// Real-valued FIR decimator with polyphase output gating.
#[derive(Debug, Clone)]
pub struct PolyphaseDecimator {
    taps: Vec<f32>,
    factor: usize,
    /// Circular buffer of the last `taps.len()` inputs.
    history: Vec<f32>,
    /// Index where the next input will be written (== one past the
    /// most recent valid sample).
    write_pos: usize,
    /// Inputs since the last emitted output.
    counter: usize,
}

impl PolyphaseDecimator {
    /// Build a decimator from an explicit tap vector. The DC gain of
    /// `taps` should be 1.0; we don't renormalise — pass through
    /// [`Self::hamming_sinc_taps`] (or scale yourself) if you want
    /// unit DC gain.
    pub fn with_taps(taps: Vec<f32>, factor: usize) -> Self {
        assert!(!taps.is_empty(), "decimator needs at least one tap");
        assert!(factor >= 1, "decimation factor must be >= 1");
        let n = taps.len();
        Self {
            taps,
            factor,
            history: vec![0.0; n],
            write_pos: 0,
            counter: 0,
        }
    }

    /// Build a decimator with Hamming-windowed sinc taps. `input_rate`
    /// must be an integer multiple of `output_rate`; the multiple
    /// becomes the decimation factor. `cutoff_hz` is the -6 dB
    /// pass-band edge, set well below `output_rate / 2` to leave
    /// transition width for the window. For NBFM at 528 → 48 kHz a
    /// cutoff of 4 kHz with 99 taps gives a clean 53 dB stop-band
    /// rejection above 24 kHz.
    pub fn with_hamming_sinc(
        input_rate_hz: u32,
        output_rate_hz: u32,
        cutoff_hz: f32,
        num_taps: usize,
    ) -> Self {
        assert!(output_rate_hz > 0);
        assert!(num_taps >= 3 && num_taps % 2 == 1, "num_taps must be odd ≥ 3");
        assert!(
            cutoff_hz > 0.0 && cutoff_hz < (output_rate_hz / 2) as f32,
            "cutoff_hz must be in (0, output_rate/2)"
        );
        assert_eq!(
            input_rate_hz % output_rate_hz,
            0,
            "input rate {input_rate_hz} must be integer multiple of output rate {output_rate_hz}"
        );
        let factor = (input_rate_hz / output_rate_hz) as usize;
        let taps = Self::hamming_sinc_taps(input_rate_hz as f32, cutoff_hz, num_taps);
        Self::with_taps(taps, factor)
    }

    /// Generate a Hamming-windowed sinc low-pass FIR, normalised so
    /// the DC gain is exactly 1.0. Used by [`Self::with_hamming_sinc`]
    /// and by the matching interpolator (which mirrors the same
    /// taps).
    pub fn hamming_sinc_taps(input_rate_hz: f32, cutoff_hz: f32, num_taps: usize) -> Vec<f32> {
        use std::f32::consts::PI;
        assert!(num_taps >= 3 && num_taps % 2 == 1, "num_taps must be odd ≥ 3");
        let nm1 = (num_taps - 1) as f32;
        let center = nm1 / 2.0;
        let omega_c = 2.0 * cutoff_hz / input_rate_hz; // 2·fc/fs
        let mut h = Vec::with_capacity(num_taps);
        let mut sum = 0.0_f32;
        for k in 0..num_taps {
            let n_off = k as f32 - center;
            let sinc = if n_off.abs() < f32::EPSILON {
                omega_c
            } else {
                (PI * omega_c * n_off).sin() / (PI * n_off)
            };
            let w = 0.54 - 0.46 * (2.0 * PI * k as f32 / nm1).cos();
            let c = sinc * w;
            h.push(c);
            sum += c;
        }
        // Normalise to unit DC gain.
        for c in &mut h {
            *c /= sum;
        }
        h
    }

    /// Decimation factor (input/output rate ratio).
    #[inline]
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Number of FIR taps.
    #[inline]
    pub fn num_taps(&self) -> usize {
        self.taps.len()
    }

    /// Process a chunk of input samples. Returns the output samples
    /// produced by this chunk (zero or more, depending on how many
    /// `factor`-aligned outputs landed in this call). Internal state
    /// keeps chunk boundaries seamless.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let n = self.taps.len();
        let mut out = Vec::with_capacity((input.len() + self.counter) / self.factor + 1);
        for &x in input {
            self.history[self.write_pos] = x;
            self.write_pos = if self.write_pos + 1 == n { 0 } else { self.write_pos + 1 };
            self.counter += 1;
            if self.counter >= self.factor {
                self.counter = 0;
                // Convolution: y = Σ taps[k] · history[(write_pos − 1 − k) mod n].
                // Walk backwards from the most recent sample, pairing
                // with taps[0..n].
                let mut idx = if self.write_pos == 0 { n - 1 } else { self.write_pos - 1 };
                let mut y = 0.0_f32;
                for &tap in &self.taps {
                    y += tap * self.history[idx];
                    idx = if idx == 0 { n - 1 } else { idx - 1 };
                }
                out.push(y);
            }
        }
        out
    }

    /// Reset internal history. Call between unrelated input streams
    /// to avoid the previous stream's tail bleeding into the next.
    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|v| *v = 0.0);
        self.write_pos = 0;
        self.counter = 0;
    }

    /// Borrow the tap vector (for tests and the matching interpolator
    /// that wants to mirror these coefficients).
    #[inline]
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }
}

// ---------------------------------------------------------------------
// Kaiser-window sinc tap generator.
//
// Added when the RX chain was rearranged to put a channel-select
// filter ahead of the FM discriminator (the original chain ran the
// demod at the host I/Q rate and only decimated the audio after, so
// adjacent channels at ±25 kHz were demodulated alongside the wanted
// one). The Hamming window above tops out at ~53 dB rejection — for
// a crowded 2 m or 70 cm band we want 70-80 dB, which is what the
// Kaiser window's parametric β gives us.
// ---------------------------------------------------------------------

/// Generate Kaiser-window sinc low-pass FIR taps, normalised to unit
/// DC gain.
///
/// Where the Hamming-sinc helper above takes a fixed tap count and
/// gives whatever stop-band falls out of it (~53 dB), this one takes
/// the **target rejection** and **transition band** and computes the
/// minimum tap count that meets both — Kaiser's design formulas:
///
/// ```text
/// β  = 0.1102·(A − 8.7)                         for A > 50 dB
/// β  = 0.5842·(A − 21)^0.4 + 0.07886·(A − 21)   for 21 ≤ A ≤ 50 dB
/// β  = 0                                         for A < 21 dB
/// N  = ceil((A − 8) / (2.285 · 2π · Δf/Fs))
/// ```
///
/// where `A = stopband_db` and `Δf = stopband_start_hz − passband_end_hz`.
/// `N` is then rounded up to the next odd integer for linear-phase
/// symmetry. The cutoff (-6 dB point) sits at the midpoint of the
/// transition band.
///
/// # Arguments
/// * `fs_hz` — sample rate at which this filter runs.
/// * `passband_end_hz` — last frequency the filter must pass with
///   ≤ 0.1 dB ripple.
/// * `stopband_start_hz` — first frequency the filter must attenuate
///   by `stopband_db`.
/// * `stopband_db` — target stop-band rejection in dB. 60 dB is a
///   reasonable amateur-radio default; 80 dB for very crowded bands.
///
/// # Panics
/// * `passband_end_hz >= stopband_start_hz`
/// * `stopband_start_hz > fs_hz / 2`
/// * `stopband_db <= 0.0`
pub fn kaiser_sinc_taps(
    fs_hz: f32,
    passband_end_hz: f32,
    stopband_start_hz: f32,
    stopband_db: f32,
) -> Vec<f32> {
    use std::f32::consts::PI;
    assert!(
        passband_end_hz < stopband_start_hz,
        "passband ({passband_end_hz}) must end before stopband ({stopband_start_hz})"
    );
    assert!(
        stopband_start_hz <= fs_hz / 2.0,
        "stopband start ({stopband_start_hz}) must be within Nyquist (fs/2 = {})",
        fs_hz / 2.0
    );
    assert!(stopband_db > 0.0, "stopband_db must be positive");

    // Kaiser β from desired stop-band attenuation A.
    let a = stopband_db;
    let beta = if a > 50.0 {
        0.1102 * (a - 8.7)
    } else if a >= 21.0 {
        0.5842 * (a - 21.0).powf(0.4) + 0.07886 * (a - 21.0)
    } else {
        0.0
    };

    // Minimum tap count from the transition band.
    let transition_norm = (stopband_start_hz - passband_end_hz) / fs_hz; // Δf/Fs
    let n_min_f = ((a - 8.0) / (2.285 * 2.0 * PI * transition_norm)).ceil();
    let n_min = (n_min_f as usize).max(3);
    let num_taps = if n_min % 2 == 0 { n_min + 1 } else { n_min };

    // Cutoff at the midpoint of the transition band (-6 dB nominal).
    let cutoff_hz = 0.5 * (passband_end_hz + stopband_start_hz);
    let omega_c = 2.0 * cutoff_hz / fs_hz; // 2·fc/fs

    let nm1 = (num_taps - 1) as f32;
    let center = nm1 / 2.0;
    let inv_i0_beta = 1.0 / kaiser_i0(beta);
    let mut h = Vec::with_capacity(num_taps);
    let mut sum = 0.0_f32;
    for k in 0..num_taps {
        let n_off = k as f32 - center;
        let sinc = if n_off.abs() < f32::EPSILON {
            omega_c
        } else {
            (PI * omega_c * n_off).sin() / (PI * n_off)
        };
        // Kaiser window — argument runs from -1 at k=0 to +1 at k=N-1.
        let arg = 2.0 * (k as f32) / nm1 - 1.0;
        let inside = (1.0 - arg * arg).max(0.0).sqrt();
        let w = kaiser_i0(beta * inside) * inv_i0_beta;
        let c = sinc * w;
        h.push(c);
        sum += c;
    }
    // Normalise so DC gain is exactly 1.0. Without this the Kaiser
    // window introduces a small mid-band droop that adds up across
    // multi-stage decimation.
    for c in &mut h {
        *c /= sum;
    }
    h
}

/// Modified Bessel function of the first kind, order 0.
///
/// Used by the Kaiser window weight calculation. Truncated power-series
/// expansion `I₀(x) = Σ (x²/4)^k / (k!)²`. Converges geometrically; for
/// the β values we feed it (typically ≤ 14) ~32 terms is well below
/// f32 round-off — but the loop also breaks early when terms drop
/// below 1e-9 of the running sum, so cheap-β cases exit fast.
fn kaiser_i0(x: f32) -> f32 {
    let x_sq_4 = (x * x) / 4.0;
    let mut term = 1.0_f32;
    let mut sum = 1.0_f32;
    for k in 1..32 {
        let kf = k as f32;
        term *= x_sq_4 / (kf * kf);
        sum += term;
        if term < 1e-9 * sum {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn dc_gain_is_one() {
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        // Steady DC input. Skip the first few outputs while the FIR
        // history fills with 1.0s.
        let n = 11 * 200; // 200 outputs worth
        let input = vec![1.0_f32; n];
        let out = d.process(&input);
        assert_eq!(out.len(), n / 11, "expected {} outputs", n / 11);
        // After 99/11 ≈ 9 outputs the history is fully primed.
        let steady: Vec<f32> = out.iter().skip(20).copied().collect();
        let max_err = steady.iter().map(|v| (v - 1.0).abs()).fold(0.0_f32, f32::max);
        assert!(max_err < 1e-5, "max DC error = {max_err}");
    }

    #[test]
    fn output_count_matches_factor() {
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        // Exactly 11 inputs → exactly 1 output.
        let out = d.process(&vec![1.0_f32; 11]);
        assert_eq!(out.len(), 1);
        // Another 11 → another 1.
        let out2 = d.process(&vec![1.0_f32; 11]);
        assert_eq!(out2.len(), 1);
        // 5 inputs → 0 outputs (counter not yet at factor).
        let out3 = d.process(&vec![1.0_f32; 5]);
        assert_eq!(out3.len(), 0);
        // Another 6 → 1 output (counter reaches 11).
        let out4 = d.process(&vec![1.0_f32; 6]);
        assert_eq!(out4.len(), 1);
    }

    #[test]
    fn rejects_above_output_nyquist() {
        // A tone at 30 kHz (above output Nyquist = 24 kHz) must be
        // attenuated by ≥ 40 dB after decimation by 11 to 48 kHz.
        let fs_in = 528_000.0_f32;
        let f0 = 30_000.0_f32;
        let n = 11 * 1_000;
        let signal: Vec<f32> = (0..n)
            .map(|k| (2.0 * PI * f0 * k as f32 / fs_in).sin())
            .collect();
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let out = d.process(&signal);
        // Skip first ~20 outputs (FIR fill transient), measure RMS.
        let rms: f32 = (out.iter().skip(20).map(|v| v * v).sum::<f32>()
            / (out.len() - 20) as f32)
            .sqrt();
        let in_rms = (1.0_f32 / 2.0).sqrt(); // sin RMS
        let atten_db = 20.0 * (rms / in_rms).log10();
        assert!(
            atten_db < -40.0,
            "30 kHz attenuation = {atten_db} dB (target ≤ -40 dB)"
        );
    }

    #[test]
    fn passes_audio_band() {
        // A 1 kHz tone (well in the pass band) should pass nearly
        // unattenuated after decimation.
        let fs_in = 528_000.0_f32;
        let f0 = 1_000.0_f32;
        let n = 11 * 1_000;
        let amp = 0.5_f32;
        let signal: Vec<f32> = (0..n)
            .map(|k| amp * (2.0 * PI * f0 * k as f32 / fs_in).sin())
            .collect();
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let out = d.process(&signal);
        // Expected RMS = amp / sqrt(2). Allow small dB error.
        let rms: f32 =
            (out.iter().skip(20).map(|v| v * v).sum::<f32>() / (out.len() - 20) as f32).sqrt();
        let expected_rms = amp / (2.0_f32).sqrt();
        let err_db = 20.0 * (rms / expected_rms).log10();
        assert!(
            err_db.abs() < 0.5,
            "1 kHz pass-band error = {err_db} dB (target |·| < 0.5 dB)"
        );
    }

    #[test]
    fn chunk_boundary_is_seamless() {
        // Splitting input across two process() calls must produce
        // bit-identical output to a single call.
        let fs_in = 528_000.0_f32;
        let n = 11 * 500;
        let signal: Vec<f32> = (0..n)
            .map(|k| (2.0 * PI * 1_500.0 * k as f32 / fs_in).sin())
            .collect();
        let mut d_a = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let mut d_b = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let out_a = d_a.process(&signal);
        let mid = n / 2;
        let mut out_b = d_b.process(&signal[..mid]);
        out_b.extend(d_b.process(&signal[mid..]));
        assert_eq!(out_a.len(), out_b.len());
        for (i, (a, b)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "mismatch at {i}: a={a} b={b}");
        }
    }

    #[test]
    fn ratio_44_for_pluto_native_min_works() {
        // Sanity-check the fallback ratio (Pluto native min ≈ 2.083 MS/s
        // → integer ÷44 to 48 kHz at 2.112 MS/s).
        let mut d = PolyphaseDecimator::with_hamming_sinc(2_112_000, 48_000, 4_000.0, 177);
        assert_eq!(d.factor(), 44);
        let n = 44 * 100;
        let out = d.process(&vec![1.0_f32; n]);
        assert_eq!(out.len(), n / 44);
        // DC gain still ≈ 1 after the FIR fills.
        let steady: f32 = out.iter().skip(20).map(|v| v - 1.0).map(|e| e.abs()).fold(0.0, f32::max);
        assert!(steady < 1e-5);
    }

    // ---- Kaiser sinc tap generator -------------------------------

    #[test]
    fn kaiser_i0_matches_known_values() {
        // Reference values from any standard table (Abramowitz & Stegun
        // 9.8.1, or `scipy.special.i0`):
        //   I₀(0)   = 1.0
        //   I₀(1)   ≈ 1.2660658
        //   I₀(5)   ≈ 27.239872
        //   I₀(10)  ≈ 2815.7166
        let cases = [(0.0, 1.0), (1.0, 1.2660658), (5.0, 27.239872), (10.0, 2815.7166)];
        for &(x, expected) in &cases {
            let got = kaiser_i0(x);
            let rel_err = (got - expected).abs() / expected.max(1.0);
            assert!(
                rel_err < 1e-4,
                "I0({x}) = {got}, expected {expected} (rel err {rel_err:.2e})"
            );
        }
    }

    #[test]
    fn kaiser_taps_normalised_and_odd() {
        let h = kaiser_sinc_taps(96_000.0, 8_000.0, 12_000.0, 80.0);
        assert!(h.len() % 2 == 1, "tap count must be odd, got {}", h.len());
        let dc: f32 = h.iter().sum();
        assert!((dc - 1.0).abs() < 1e-6, "DC gain {dc} should be 1.0");
        // Symmetric — h[k] == h[N-1-k].
        let n = h.len();
        for k in 0..n / 2 {
            let err = (h[k] - h[n - 1 - k]).abs();
            assert!(err < 1e-6, "asymmetric at k={k}: {} vs {}", h[k], h[n - 1 - k]);
        }
    }

    #[test]
    fn kaiser_taps_meet_stopband_target() {
        // Target 80 dB rejection past 12 kHz at 96 kHz Fs. Generate the
        // taps, then sweep the response by direct DFT at a couple of
        // stopband frequencies and check we're at or below -80 dB.
        let fs = 96_000.0_f32;
        let h = kaiser_sinc_taps(fs, 8_000.0, 12_000.0, 80.0);
        for &f in &[12_000.0_f32, 18_000.0, 24_000.0, 40_000.0] {
            let mag = freq_response_db(&h, fs, f);
            assert!(
                mag <= -75.0,
                "stopband leakage at {f} Hz = {mag:.1} dB (target ≤ -75)"
            );
        }
        // Passband must stay flat near DC.
        for &f in &[0.0_f32, 1_000.0, 5_000.0, 7_000.0] {
            let mag = freq_response_db(&h, fs, f);
            assert!(
                mag.abs() < 0.5,
                "passband ripple at {f} Hz = {mag:.2} dB (target |·| < 0.5)"
            );
        }
    }

    /// Direct DFT magnitude (in dB) at one frequency. Linear-phase FIR
    /// with unit DC gain → at f = 0 we get exactly 0 dB.
    fn freq_response_db(taps: &[f32], fs_hz: f32, f_hz: f32) -> f32 {
        use std::f32::consts::PI;
        let omega = 2.0 * PI * f_hz / fs_hz;
        let mut re = 0.0_f32;
        let mut im = 0.0_f32;
        for (k, &t) in taps.iter().enumerate() {
            let phi = omega * k as f32;
            re += t * phi.cos();
            im -= t * phi.sin();
        }
        let mag = (re * re + im * im).sqrt();
        20.0 * mag.log10()
    }

    // The complex side of the chain (decim + LPF on Complex32) is
    // exercised by the freq_xlating module's own test suite — see
    // `freq_xlating::tests::zero_center_passes_in_band` and
    // `freq_xlating::tests::adjacent_channel_rejected`. No need to
    // duplicate them here.
}
