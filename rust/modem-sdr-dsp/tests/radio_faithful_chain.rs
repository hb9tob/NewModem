//! Integration test — radio-faithful TX→RX chain.
//!
//! Reproduces what a real ham NBFM transceiver does end-to-end:
//!
//! ```text
//!   48 kHz audio
//!     → interp ×11 to 528 kHz
//!     → PhaseMod (k_p = 5)                         ← TX = PM, no preemph filter
//!   I/Q at 528 kHz                                  ← what Pluto would carry
//!     → QuadratureDemod (FM discriminator)
//!     → DeemphasisLpf (corner 300 Hz)              ← undoes PM's +6 dB/oct
//!     → SubAudioHpf (corner 300 Hz)                ← CTCSS reject, kills < 300 Hz
//!     → decim ÷11
//!   48 kHz audio (recovered)
//! ```
//!
//! Distinguishing feature: anything below ~300 Hz in the input is
//! suppressed in the output (the HPF), and the 300 Hz – 3 kHz audio
//! band is preserved up to a calibration-dependent overall gain
//! factor (`k_p · corner_hz / max_dev` per the deemph integrator
//! math). When we have measured radio data we'll calibrate `k_p`
//! and the corners against it.

use modem_sdr_dsp::{
    audio_filters::{DeemphasisLpf, SubAudioHpf},
    decimator::PolyphaseDecimator,
    fm_demod::QuadratureDemod,
    interpolator::PolyphaseInterpolator,
    pm_mod::PhaseMod,
    AUDIO_RATE, MAX_DEVIATION_HZ,
};
use std::f32::consts::PI;

const IF_RATE: u32 = 528_000;
const FIR_TAPS: usize = 99;
const FIR_CUTOFF_HZ: f32 = 12_000.0; // wider than audio band, post-PM bandwidth fits comfortably

/// Run a single tone at frequency `f_audio` Hz through the
/// radio-faithful chain and return the input/output RMS pair.
/// Both RMS values measured over the steady-state portion (first
/// 1500 audio samples skipped, that's well past every IIR/FIR
/// fill transient on this chain).
fn chain_rms(f_audio: f32, amp: f32) -> (f32, f32) {
    let n = 16_384;
    let audio_in: Vec<f32> = (0..n)
        .map(|k| amp * (2.0 * PI * f_audio * k as f32 / AUDIO_RATE as f32).sin())
        .collect();

    let mut interp =
        PolyphaseInterpolator::with_hamming_sinc(AUDIO_RATE, IF_RATE, FIR_CUTOFF_HZ, FIR_TAPS);
    let if_audio = interp.process(&audio_in);

    let pm = PhaseMod::calibrated();
    let iq = pm.process_alloc(&if_audio);

    let mut demod = QuadratureDemod::new(IF_RATE as f32, MAX_DEVIATION_HZ);
    let mut audio_if = demod.process_alloc(&iq);

    let mut deemph = DeemphasisLpf::calibrated(IF_RATE as f32);
    deemph.process(&mut audio_if);

    let mut hpf = SubAudioHpf::calibrated(IF_RATE as f32);
    hpf.process(&mut audio_if);

    let mut decim =
        PolyphaseDecimator::with_hamming_sinc(IF_RATE, AUDIO_RATE, FIR_CUTOFF_HZ, FIR_TAPS);
    let audio_out = decim.process(&audio_if);

    let skip = 1_500;
    let in_rms = (audio_in[skip..].iter().map(|v| v * v).sum::<f32>()
        / (audio_in.len() - skip) as f32)
        .sqrt();
    let out_rms = (audio_out[skip..].iter().map(|v| v * v).sum::<f32>()
        / (audio_out.len() - skip) as f32)
        .sqrt();
    (in_rms, out_rms)
}

/// Closed-form prediction of the chain gain at frequency `f`,
/// from the cascade of:
/// - PM gain at `f` (relative to max-dev): `k_p · f / max_dev`
/// - Deemph LPF: `1 / √(1 + (f/fc)²)`
/// - Sub-audio HPF: `(f/fc) / √(1 + (f/fc)²)`
///
/// The interp/decim FIRs have unity passband response at audio
/// frequencies (verified by their own tests), so we don't fold
/// them into this prediction. Used to set tight assertions and
/// document the radio-faithful chain's response curve.
fn predicted_chain_gain(f: f32) -> f32 {
    let k_p = PhaseMod::DEFAULT_K_P;
    let fc_lpf = DeemphasisLpf::DEFAULT_CORNER_HZ;
    let fc_hpf = SubAudioHpf::DEFAULT_CORNER_HZ;
    let pm_gain = k_p * f / MAX_DEVIATION_HZ;
    let lpf_mag = 1.0 / (1.0 + (f / fc_lpf).powi(2)).sqrt();
    let hpf_mag = (f / fc_hpf) / (1.0 + (f / fc_hpf).powi(2)).sqrt();
    pm_gain * lpf_mag * hpf_mag
}

/// 1 kHz tone (well in-band): output scale matches the closed-form
/// chain-gain prediction within < 0.3 dB.
#[test]
fn one_khz_tone_matches_predicted_chain_gain() {
    let (in_rms, out_rms) = chain_rms(1_000.0, 0.1);
    let scale = out_rms / in_rms;
    // Predicted at 1 kHz, k_p=5, fc_lpf=fc_hpf=300, max_dev=5000:
    //   pm   = 1.000
    //   lpf  = 0.2873  (-10.85 dB)
    //   hpf  = 0.9578  ( -0.37 dB)
    //   net  = 0.2752  (-11.21 dB)
    let expected = predicted_chain_gain(1_000.0);
    let err_db = 20.0 * (scale / expected).log10();
    assert!(
        err_db.abs() < 0.3,
        "1 kHz scale = {scale} (predicted {expected}), err = {err_db} dB"
    );
}

/// 200 Hz tone (below the 300 Hz HPF corner): clearly suppressed.
/// At 200 Hz the cascade is:
///   pm_gain = 5·200/5000 = 0.20  (-14.0 dB vs DC reference)
///   lpf     = 1/√(1+(200/300)²) = 0.832  (-1.6 dB)
///   hpf     = (200/300)/√(1+(200/300)²) = 0.555 (-5.1 dB)
///   total   = 0.0922 (-20.7 dB on absolute scale)
/// At 1 kHz the total was 0.275 (-11.2 dB), so the *ratio* of
/// 200 Hz to 1 kHz output is -20.7 - (-11.2) = -9.5 dB. The
/// "300 Hz onset" the user sees in real radio loopback is exactly
/// this knee.
#[test]
fn two_hundred_hz_is_clearly_below_one_khz() {
    let (_, out_rms_200) = chain_rms(200.0, 0.1);
    let (_, out_rms_1k) = chain_rms(1_000.0, 0.1);
    let ratio_db = 20.0 * (out_rms_200 / out_rms_1k).log10();
    let expected = 20.0 * (predicted_chain_gain(200.0) / predicted_chain_gain(1_000.0)).log10();
    let err_db = ratio_db - expected;
    assert!(
        err_db.abs() < 0.3,
        "200/1k ratio = {ratio_db} dB (predicted {expected} dB), err = {err_db} dB"
    );
    // Sanity: the ratio is negative and big enough that 200 Hz
    // is unambiguously below the audio passband.
    assert!(ratio_db < -8.0, "200 Hz not clearly below 1 kHz: {ratio_db} dB");
}

/// 2 kHz tone (in-band, above the deemph corner): output gain
/// should match the same calibration scale as 1 kHz (the deemph
/// integrator perfectly undoes the PM derivative across the band).
#[test]
fn two_khz_tone_matches_one_khz() {
    let (_, out_rms_1k) = chain_rms(1_000.0, 0.1);
    let (_, out_rms_2k) = chain_rms(2_000.0, 0.1);
    // The chain is mathematically flat above the deemph corner —
    // both should give the same output RMS. Allow ±1 dB for the
    // deemph asymptote not being perfectly flat at finite f/fc.
    let ratio_db = 20.0 * (out_rms_2k / out_rms_1k).log10();
    assert!(
        ratio_db.abs() < 1.0,
        "2 kHz vs 1 kHz ratio = {ratio_db} dB (target |·| < 1 dB, ideal 0)"
    );
}

/// Sweep across the audio band and emit a quick frequency response
/// table. Doesn't assert hard bounds — it's documentation of the
/// chain's actual response for human review and future
/// regression-snapshot. Run with `cargo test -p modem-sdr-dsp
/// --test radio_faithful_chain print_freq_response -- --nocapture`.
#[test]
fn print_freq_response() {
    let freqs = [
        100.0_f32, 150.0, 200.0, 250.0, 300.0, 400.0, 500.0, 700.0, 1_000.0, 1_500.0, 2_000.0,
        2_500.0, 3_000.0,
    ];
    let (_, ref_rms) = chain_rms(1_000.0, 0.1);
    println!("\n  freq (Hz)  |  rel to 1 kHz");
    println!(" -----------+----------------");
    for &f in &freqs {
        let (_, out) = chain_rms(f, 0.1);
        let rel_db = 20.0 * (out / ref_rms).log10();
        println!(" {f:>9.0}  |  {rel_db:>+7.2} dB");
    }
}
