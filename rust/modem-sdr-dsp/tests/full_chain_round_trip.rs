//! Integration test — full SDR DSP chain round-trip.
//!
//! Mirrors the planned TX → RX path end-to-end at the level of
//! abstraction the SDR backend crates (`modem-pluto`,
//! `modem-rtlsdr`, `modem-sdrplay`) will see:
//!
//! ```text
//!   48 kHz audio
//!     → emphasis::preemphasis_nbfm_48k
//!     → interpolator::PolyphaseInterpolator (×11)
//!     → fm_mod::FrequencyMod              [I/Q at 528 kHz]
//!     → fm_demod::QuadratureDemod         [audio at 528 kHz]
//!     → decimator::PolyphaseDecimator    (÷11)
//!     → emphasis::DeemphasisFilter
//!   48 kHz audio (recovered)
//! ```
//!
//! The whole cascade is the mathematical identity, modulo:
//! - polyphase FIR group delay (~9 audio samples)
//! - emphasis IIR settling (first ~36 audio samples)
//! - FIR stop-band leakage (~50 dB ripple from Hamming window)
//!
//! Pass criterion: recovered RMS matches input RMS within 0.5 dB
//! after skipping the cascade transient. Phase / group-delay
//! alignment is intentionally not asserted — `modem-core` doesn't
//! care about audio-domain phase, only the demodulated symbols.

use modem_sdr_dsp::{
    decimator::PolyphaseDecimator,
    emphasis::{preemphasis_nbfm_48k, DeemphasisFilter},
    fm_demod::QuadratureDemod,
    fm_mod::FrequencyMod,
    interpolator::PolyphaseInterpolator,
    AUDIO_RATE, MAX_DEVIATION_HZ,
};
use std::f32::consts::PI;

const IF_RATE: u32 = 528_000; // Pluto with 4× decim FIR loaded
const FIR_TAPS: usize = 99;
const FIR_CUTOFF_HZ: f32 = 4_000.0;

fn rms(samples: &[f32]) -> f32 {
    let sum_sq: f32 = samples.iter().map(|v| v * v).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[test]
fn round_trip_preserves_audio_amplitude() {
    // Single 1 kHz tone, low enough amplitude that preemph (+13 dB
    // at 1 kHz) doesn't push the FM modulator past full deviation.
    let fs_audio = AUDIO_RATE as f32;
    let n = 8_192;
    let amp = 0.1_f32;
    let signal_in: Vec<f32> = (0..n)
        .map(|k| amp * (2.0 * PI * 1_000.0 * k as f32 / fs_audio).sin())
        .collect();

    // ── TX chain ───────────────────────────────────────────────
    let mut audio_tx = signal_in.clone();
    preemphasis_nbfm_48k(&mut audio_tx);

    let mut interp =
        PolyphaseInterpolator::with_hamming_sinc(AUDIO_RATE, IF_RATE, FIR_CUTOFF_HZ, FIR_TAPS);
    let if_real = interp.process(&audio_tx);
    assert_eq!(if_real.len(), n * 11);

    let mut modu = FrequencyMod::new(IF_RATE as f32, MAX_DEVIATION_HZ);
    let iq = modu.process_alloc(&if_real);
    assert_eq!(iq.len(), if_real.len());

    // FM is constant-envelope: confirm the I/Q sits on the unit
    // circle to within float noise.
    let max_env_err = iq
        .iter()
        .map(|c| (c.norm_sqr() - 1.0).abs())
        .fold(0.0_f32, f32::max);
    assert!(
        max_env_err < 1e-5,
        "FM envelope error from unit circle = {max_env_err}"
    );

    // ── RX chain ───────────────────────────────────────────────
    let mut demod = QuadratureDemod::new(IF_RATE as f32, MAX_DEVIATION_HZ);
    let if_demod = demod.process_alloc(&iq);

    let mut decim =
        PolyphaseDecimator::with_hamming_sinc(IF_RATE, AUDIO_RATE, FIR_CUTOFF_HZ, FIR_TAPS);
    let mut audio_rx = decim.process(&if_demod);
    assert_eq!(audio_rx.len(), n);

    let mut deemp = DeemphasisFilter::new();
    deemp.process(&mut audio_rx);

    // ── Comparison ─────────────────────────────────────────────
    // Skip the cascade fill transient (FIR + emphasis IIR settling).
    // 200 audio samples = ~5 emphasis time constants + plenty of
    // polyphase warm-up.
    let skip = 200;
    let in_rms = rms(&signal_in[skip..]);
    let out_rms = rms(&audio_rx[skip..]);
    let err_db = 20.0 * (out_rms / in_rms).log10();
    assert!(
        err_db.abs() < 0.5,
        "full-chain RMS error = {err_db} dB (target |·| < 0.5 dB), \
         in_rms={in_rms}, out_rms={out_rms}"
    );
}

/// Same round-trip with the Pluto-native fallback rate (no
/// programmable FIR loaded). Confirms the chain works equally
/// well when modem-pluto can't get 528 kS/s and falls back to
/// 2.112 MS/s × ÷44.
#[test]
fn round_trip_works_at_pluto_native_min_rate() {
    let fs_audio = AUDIO_RATE as f32;
    let n = 4_096; // smaller — IF buffer is 4× bigger at this rate
    let amp = 0.1_f32;
    let signal_in: Vec<f32> = (0..n)
        .map(|k| amp * (2.0 * PI * 800.0 * k as f32 / fs_audio).sin())
        .collect();

    let if_rate: u32 = 2_112_000;
    let taps = 177; // multiple of 44 for clean polyphase decomposition

    let mut audio_tx = signal_in.clone();
    preemphasis_nbfm_48k(&mut audio_tx);

    let mut interp =
        PolyphaseInterpolator::with_hamming_sinc(AUDIO_RATE, if_rate, FIR_CUTOFF_HZ, taps);
    let if_real = interp.process(&audio_tx);

    let mut modu = FrequencyMod::new(if_rate as f32, MAX_DEVIATION_HZ);
    let iq = modu.process_alloc(&if_real);

    let mut demod = QuadratureDemod::new(if_rate as f32, MAX_DEVIATION_HZ);
    let if_demod = demod.process_alloc(&iq);

    let mut decim =
        PolyphaseDecimator::with_hamming_sinc(if_rate, AUDIO_RATE, FIR_CUTOFF_HZ, taps);
    let mut audio_rx = decim.process(&if_demod);

    let mut deemp = DeemphasisFilter::new();
    deemp.process(&mut audio_rx);

    assert_eq!(audio_rx.len(), n);

    let skip = 300;
    let in_rms = rms(&signal_in[skip..]);
    let out_rms = rms(&audio_rx[skip..]);
    let err_db = 20.0 * (out_rms / in_rms).log10();
    assert!(
        err_db.abs() < 0.5,
        "fallback-rate full-chain RMS error = {err_db} dB"
    );
}

/// Multi-tone audio (200 Hz + 1 kHz + 2 kHz) — every component is
/// well in-band so the chain should preserve amplitude. Uses a
/// wider FIR cutoff (12 kHz) than the single-tone test because at
/// 99 taps the Hamming transition width is ≈ 17.6 kHz at 528 kSa/s,
/// which puts a 4 kHz cutoff right in the middle of the transition
/// and starts attenuating the 2 kHz component (≈ 0.3 dB per filter,
/// 0.6 dB cascaded). With cutoff = 12 kHz the audio band sits
/// firmly in the pass band and the cascade is < 0.1 dB-tilt across
/// 200 Hz – 2 kHz. Real modem-pluto operation can pick the cutoff
/// per use case (narrow for SNR, wide for amplitude fidelity).
#[test]
fn round_trip_preserves_multi_tone_amplitude() {
    let fs_audio = AUDIO_RATE as f32;
    let n = 8_192;
    let amp = 0.05_f32; // each component 0.05 → peak 0.15 → safe
    let cutoff = 12_000.0_f32;
    let signal_in: Vec<f32> = (0..n)
        .map(|k| {
            let t = k as f32 / fs_audio;
            amp * ((2.0 * PI * 200.0 * t).sin()
                + (2.0 * PI * 1_000.0 * t).sin()
                + (2.0 * PI * 2_000.0 * t).sin())
        })
        .collect();

    let mut audio_tx = signal_in.clone();
    preemphasis_nbfm_48k(&mut audio_tx);
    let mut interp =
        PolyphaseInterpolator::with_hamming_sinc(AUDIO_RATE, IF_RATE, cutoff, FIR_TAPS);
    let if_real = interp.process(&audio_tx);
    let mut modu = FrequencyMod::new(IF_RATE as f32, MAX_DEVIATION_HZ);
    let iq = modu.process_alloc(&if_real);
    let mut demod = QuadratureDemod::new(IF_RATE as f32, MAX_DEVIATION_HZ);
    let if_demod = demod.process_alloc(&iq);
    let mut decim =
        PolyphaseDecimator::with_hamming_sinc(IF_RATE, AUDIO_RATE, cutoff, FIR_TAPS);
    let mut audio_rx = decim.process(&if_demod);
    let mut deemp = DeemphasisFilter::new();
    deemp.process(&mut audio_rx);

    let skip = 200;
    let in_rms = rms(&signal_in[skip..]);
    let out_rms = rms(&audio_rx[skip..]);
    let err_db = 20.0 * (out_rms / in_rms).log10();
    assert!(
        err_db.abs() < 0.5,
        "multi-tone full-chain RMS error = {err_db} dB"
    );
}
