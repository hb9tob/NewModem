//! Modulateur passband complet : bytes -> audio reel a 48 kHz.

use num_complex::Complex32;
use crate::{
    AUDIO_RATE, DATA_CENTER_HZ, ROLLOFF, ModemMode,
    constellation::{psk8, qam16, qam32_cross, bits_to_symbols},
    framing::{preamble, interleave_data_pilots},
    ldpc::LdpcEncoder,
    rrc::{rrc_taps, upsample, convolve_complex_real},
};

pub fn constellation_for(mode: ModemMode) -> (Vec<Complex32>, Vec<u8>) {
    match mode {
        ModemMode::Psk8R12_1500 | ModemMode::Psk8R12_500 => psk8(),
        ModemMode::Qam16R12_1600 | ModemMode::Qam16R34_1500 => qam16(),
        ModemMode::Qam32R34_1200 => qam32_cross(),
    }
}

/// Convertit un bloc d'octets en bits (MSB first).
pub fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(bytes.len() * 8);
    for b in bytes {
        for k in (0..8).rev() {
            bits.push((b >> k) & 1);
        }
    }
    bits
}

/// Pipeline complet TX :
///   bytes -> bits -> LDPC encode -> symboles -> pilotes TDM -> RRC -> upmix -> audio
pub fn modulate_bytes(bytes: &[u8], mode: ModemMode, peak_target: f32) -> Vec<f32> {
    // 1. bytes -> bits info
    let info_bits = bytes_to_bits(bytes);

    // 2. LDPC encode
    let encoder = LdpcEncoder::new(mode.ldpc_code());
    let coded_bits = encoder.encode_padded(&info_bits);

    // 3. bits -> symboles. Padding eventuel a un multiple de bits/symbol.
    let bps = mode.bits_per_symbol();
    let extra = (bps - coded_bits.len() % bps) % bps;
    let mut padded = coded_bits;
    padded.extend(std::iter::repeat(0u8).take(extra));

    let (constellation, bits_map) = constellation_for(mode);
    let data_syms = bits_to_symbols(&padded, &constellation, &bits_map, bps);

    modulate_symbols(&data_syms, mode, peak_target)
}

/// Variante sans FEC : bits passent directement (pour debug ou tests sans
/// codage). Pas pour usage en transmission reelle.
pub fn modulate_raw_bits(bits: &[u8], mode: ModemMode, peak_target: f32) -> Vec<f32> {
    let bps = mode.bits_per_symbol();
    let extra = (bps - bits.len() % bps) % bps;
    let mut padded = bits.to_vec();
    padded.extend(std::iter::repeat(0u8).take(extra));
    let (constellation, bits_map) = constellation_for(mode);
    let data_syms = bits_to_symbols(&padded, &constellation, &bits_map, bps);
    modulate_symbols(&data_syms, mode, peak_target)
}

/// A partir des symboles data deja prets : ajoute preambule + pilotes TDM
/// + RRC + upmix + normalisation.
fn modulate_symbols(data_syms: &[Complex32], mode: ModemMode, peak_target: f32) -> Vec<f32> {
    let pre = preamble();
    let (data_with_pilots, _positions) = interleave_data_pilots(data_syms);
    let mut all_syms = pre;
    all_syms.extend_from_slice(&data_with_pilots);

    let symbol_rate = mode.symbol_rate();
    assert!(AUDIO_RATE % symbol_rate == 0,
        "AUDIO_RATE {} doit etre multiple de symbol_rate {}",
        AUDIO_RATE, symbol_rate);
    let sps = (AUDIO_RATE / symbol_rate) as usize;

    let up = upsample(&all_syms, sps);
    let taps = rrc_taps(ROLLOFF, 12, sps);
    let baseband = convolve_complex_real(&up, &taps);

    let n = baseband.len();
    let mut passband = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / AUDIO_RATE as f32;
        let phase = 2.0 * std::f32::consts::PI * DATA_CENTER_HZ * t;
        passband.push(baseband[i].re * phase.cos() - baseband[i].im * phase.sin());
    }

    let peak = passband.iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
    if peak > 0.0 {
        let scale = peak_target / peak;
        for v in &mut passband {
            *v *= scale;
        }
    }
    passband
}

/// Ton CW pour declencher le VOX.
pub fn vox_tone(duration_s: f32, amplitude: f32, freq_hz: f32) -> Vec<f32> {
    let n = (duration_s * AUDIO_RATE as f32) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / AUDIO_RATE as f32;
        out.push(amplitude * (2.0 * std::f32::consts::PI * freq_hz * t).sin());
    }
    let ramp = ((0.020 * AUDIO_RATE as f32) as usize).min(n / 2);
    for i in 0..ramp {
        let r = 0.5 * (1.0 - (std::f32::consts::PI * i as f32 / ramp as f32).cos());
        out[i] *= r;
        out[n - 1 - i] *= r;
    }
    out
}
