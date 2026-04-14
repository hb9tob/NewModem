//! Modulateur passe-bande complet : symboles -> audio reel a 48 kHz.

use num_complex::Complex32;
use crate::{
    AUDIO_RATE, DATA_CENTER_HZ, ROLLOFF, ModemMode,
    constellation::{psk8, qam16, qam32_cross, bits_to_symbols},
    framing::{preamble, interleave_data_pilots},
    rrc::{rrc_taps, upsample, convolve_complex_real},
};

pub fn constellation_for(mode: ModemMode) -> (Vec<Complex32>, Vec<u8>) {
    match mode {
        ModemMode::Psk8R12_1500 | ModemMode::Psk8R12_500 => psk8(),
        ModemMode::Qam16R12_1600 | ModemMode::Qam16R34_1500 => qam16(),
        ModemMode::Qam32R34_1200 => qam32_cross(),
    }
}

/// Genere le signal audio reel passband normalise a peak `peak_target`.
/// Pas de FEC : les bits sont mappes directement aux symboles.
///
/// Pour avoir un ensemble entier de symboles a partir des bits, padd avec
/// des zeros si necessaire.
pub fn modulate_raw_bits(bits: &[u8], mode: ModemMode, peak_target: f32) -> Vec<f32> {
    let bps = mode.bits_per_symbol();
    // Padding : completer a un multiple de bps
    let mut bits_padded = bits.to_vec();
    let extra = (bps - bits_padded.len() % bps) % bps;
    bits_padded.extend(std::iter::repeat(0).take(extra));

    let (constellation, bits_map) = constellation_for(mode);
    let data_syms = bits_to_symbols(&bits_padded, &constellation, &bits_map, bps);

    // Ajoute preambule + insere pilotes TDM
    let pre = preamble();
    let (data_with_pilots, _positions) = interleave_data_pilots(&data_syms);
    let mut all_syms = pre;
    all_syms.extend_from_slice(&data_with_pilots);

    let symbol_rate = mode.symbol_rate();
    assert!(AUDIO_RATE % symbol_rate == 0,
        "AUDIO_RATE {} doit etre multiple de symbol_rate {}",
        AUDIO_RATE, symbol_rate);
    let sps = (AUDIO_RATE / symbol_rate) as usize;

    // Upsample + RRC
    let up = upsample(&all_syms, sps);
    let taps = rrc_taps(ROLLOFF, 12, sps);
    let baseband = convolve_complex_real(&up, &taps);

    // Modulation passband : Re{baseband * exp(j*2*pi*fc*t)}
    let n = baseband.len();
    let mut passband = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / AUDIO_RATE as f32;
        let phase = 2.0 * std::f32::consts::PI * DATA_CENTER_HZ * t;
        let cos_p = phase.cos();
        let sin_p = phase.sin();
        // Re((a+jb) * (cos+jsin)) = a*cos - b*sin
        passband.push(baseband[i].re * cos_p - baseband[i].im * sin_p);
    }

    // Normalise a peak_target
    let peak = passband.iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
    if peak > 0.0 {
        let scale = peak_target / peak;
        for v in &mut passband {
            *v *= scale;
        }
    }
    passband
}

/// Ton CW pour declencher le VOX (carrier au milieu du spectre data).
/// Duree en secondes, amplitude crete, freq Hz.
pub fn vox_tone(duration_s: f32, amplitude: f32, freq_hz: f32) -> Vec<f32> {
    let n = (duration_s * AUDIO_RATE as f32) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / AUDIO_RATE as f32;
        out.push(amplitude * (2.0 * std::f32::consts::PI * freq_hz * t).sin());
    }
    // Rampe douce 20 ms aux bords pour eviter clicks
    let ramp = ((0.020 * AUDIO_RATE as f32) as usize).min(n / 2);
    for i in 0..ramp {
        let r = 0.5 * (1.0 - (std::f32::consts::PI * i as f32 / ramp as f32).cos());
        out[i] *= r;
        out[n - 1 - i] *= r;
    }
    out
}
