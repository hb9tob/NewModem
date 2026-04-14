//! Constellations 8PSK, 16QAM, 32QAM cross.
//!
//! 8PSK utilise un mapping Gray (3 bits par symbole).
//! 16QAM est square, mapping naturel (sera Gray plus tard).
//! 32QAM est cross-32QAM (carre 6x6 sans les 4 coins), mapping naturel.

use num_complex::Complex32;

pub type Symbol = Complex32;

/// 8PSK Gray-mapped : retourne la constellation et la table bits -> indice.
/// bits_to_idx[bits_value] = index dans constellation.
pub fn psk8() -> (Vec<Symbol>, Vec<u8>) {
    let mut pts = Vec::with_capacity(8);
    for k in 0..8 {
        let theta = 2.0 * std::f32::consts::PI * (k as f32) / 8.0;
        pts.push(Symbol::new(theta.cos(), theta.sin()));
    }
    // Gray code mapping : bits_map[bits_val] = constellation index
    // Comme dans le sim Python : [0, 1, 3, 2, 6, 7, 5, 4]
    let bits_map: Vec<u8> = vec![0, 1, 3, 2, 6, 7, 5, 4];
    (pts, bits_map)
}

/// 16QAM square, normalise mean power = 1. Mapping naturel (binaire).
pub fn qam16() -> (Vec<Symbol>, Vec<u8>) {
    let levels = [-3.0_f32, -1.0, 1.0, 3.0];
    let norm = (10.0_f32).sqrt();
    let mut pts = Vec::with_capacity(16);
    for &q in &levels {
        for &i in &levels {
            pts.push(Symbol::new(i / norm, q / norm));
        }
    }
    let bits_map: Vec<u8> = (0..16).collect();
    (pts, bits_map)
}

/// 32QAM cross : carre 6x6 sans les 4 coins. Mean power normalise.
pub fn qam32_cross() -> (Vec<Symbol>, Vec<u8>) {
    let levels = [-5.0_f32, -3.0, -1.0, 1.0, 3.0, 5.0];
    let norm = (20.0_f32).sqrt();
    let mut pts = Vec::with_capacity(32);
    for &q in &levels {
        for &i in &levels {
            if i.abs() == 5.0 && q.abs() == 5.0 {
                continue;
            }
            pts.push(Symbol::new(i / norm, q / norm));
        }
    }
    let bits_map: Vec<u8> = (0..32).collect();
    (pts, bits_map)
}

/// Mappe une sequence de bits en symboles via constellation + bits_map.
/// `bits` doit etre multiple de bits_per_symbol.
pub fn bits_to_symbols(
    bits: &[u8],
    constellation: &[Symbol],
    bits_map: &[u8],
    bits_per_sym: usize,
) -> Vec<Symbol> {
    assert!(bits.len() % bits_per_sym == 0,
        "bits length {} not multiple of bits_per_sym {}",
        bits.len(), bits_per_sym);
    let n_syms = bits.len() / bits_per_sym;
    let mut out = Vec::with_capacity(n_syms);
    for i in 0..n_syms {
        let mut idx_bits: u8 = 0;
        for b in 0..bits_per_sym {
            idx_bits = (idx_bits << 1) | bits[i * bits_per_sym + b];
        }
        let const_idx = bits_map[idx_bits as usize] as usize;
        out.push(constellation[const_idx]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psk8_unit_circle() {
        let (pts, _map) = psk8();
        for p in &pts {
            assert!((p.norm() - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn qam16_size() {
        let (pts, _) = qam16();
        assert_eq!(pts.len(), 16);
    }

    #[test]
    fn qam32_cross_size() {
        let (pts, _) = qam32_cross();
        assert_eq!(pts.len(), 32);
    }

    #[test]
    fn bits_roundtrip_qpsk_count() {
        let (pts, map) = psk8();
        let bits = vec![0, 1, 0,  1, 0, 1,  1, 1, 1];
        let syms = bits_to_symbols(&bits, &pts, &map, 3);
        assert_eq!(syms.len(), 3);
    }
}
