//! Constellations: QPSK Gray, 8PSK Gray, 16-APSK DVB-S2 (4,12).
//!
//! Port exact de modem_apsk16_ftn_bench.py lignes 30-245.
//! Toutes normalisées Es = 1.

use std::f64::consts::PI;

use crate::types::Complex64;

/// A constellation with bit mapping.
#[derive(Clone, Debug)]
pub struct Constellation {
    pub points: Vec<Complex64>,
    pub bits_per_sym: usize,
    /// bit_map[symbol_index][bit_position] = 0 or 1, MSB first
    pub bit_map: Vec<Vec<u8>>,
}

impl Constellation {
    /// Map a bit slice to symbols. `bits.len()` must be a multiple of `bits_per_sym`.
    pub fn map_bits(&self, bits: &[u8]) -> Vec<Complex64> {
        assert_eq!(bits.len() % self.bits_per_sym, 0);
        bits.chunks_exact(self.bits_per_sym)
            .map(|chunk| {
                let mut idx: usize = 0;
                for &b in chunk {
                    idx = (idx << 1) | (b as usize & 1);
                }
                self.points[idx]
            })
            .collect()
    }

    /// Nearest-neighbor hard decision. Returns symbol indices.
    pub fn slice_nearest(&self, y: &[Complex64]) -> Vec<usize> {
        y.iter()
            .map(|&yi| {
                let mut best = 0;
                let mut best_d2 = f64::INFINITY;
                for (k, &s) in self.points.iter().enumerate() {
                    let d2 = (yi - s).norm_sqr();
                    if d2 < best_d2 {
                        best_d2 = d2;
                        best = k;
                    }
                }
                best
            })
            .collect()
    }

    /// Nearest-neighbor hard decision for a single received symbol. Returns
    /// the closest constellation point (as a reference symbol), not its index.
    /// Used as the `decision` argument by `DdPll::derotate_and_update`.
    pub fn hard_decision(&self, y: Complex64) -> Complex64 {
        let mut best = self.points[0];
        let mut best_d2 = (y - best).norm_sqr();
        for &s in self.points.iter().skip(1) {
            let d2 = (y - s).norm_sqr();
            if d2 < best_d2 {
                best_d2 = d2;
                best = s;
            }
        }
        best
    }

    /// Convert symbol indices to bits (MSB first per symbol).
    pub fn symbols_to_bits(&self, indices: &[usize]) -> Vec<u8> {
        let mut bits = Vec::with_capacity(indices.len() * self.bits_per_sym);
        for &idx in indices {
            for k in 0..self.bits_per_sym {
                bits.push((idx >> (self.bits_per_sym - 1 - k)) as u8 & 1);
            }
        }
        bits
    }
}

/// QPSK Gray, 4 points unit-circle, Es = 1.
///
/// Mapping: 00 -> pi/4, 01 -> 3pi/4, 11 -> -3pi/4, 10 -> -pi/4.
pub fn qpsk_gray() -> Constellation {
    let mut points = vec![Complex64::new(0.0, 0.0); 4];
    points[0b00] = Complex64::from_polar(1.0, PI / 4.0);
    points[0b01] = Complex64::from_polar(1.0, 3.0 * PI / 4.0);
    points[0b11] = Complex64::from_polar(1.0, -3.0 * PI / 4.0);
    points[0b10] = Complex64::from_polar(1.0, -PI / 4.0);

    let bit_map = make_bit_map(4, 2);
    Constellation {
        points,
        bits_per_sym: 2,
        bit_map,
    }
}

/// 8PSK Gray, 8 points unit-circle, Es = 1.
///
/// Gray mapping: bit_pattern = k XOR (k >> 1), angle = k * pi/4.
pub fn psk8_gray() -> Constellation {
    let mut points = vec![Complex64::new(0.0, 0.0); 8];
    for k in 0..8u32 {
        let bit_pattern = k ^ (k >> 1);
        let angle = k as f64 * PI / 4.0;
        points[bit_pattern as usize] = Complex64::new(angle.cos(), angle.sin());
    }

    let bit_map = make_bit_map(8, 3);
    Constellation {
        points,
        bits_per_sym: 3,
        bit_map,
    }
}

/// 16-APSK (4,12) DVB-S2, normalised Es = 1.
///
/// Gamma = R2/R1 (outer/inner ring ratio). DVB-S2 table 9: gamma=2.85 for rate 3/4.
/// Indices 0..11: outer ring (r2). Indices 12..15: inner ring (r1).
/// Angles from ETSI EN 302 307-1 sect. 5.4.3 / gr-dvbs2 m_16apsk.
pub fn apsk16_dvbs2(gamma: f64) -> Constellation {
    assert!(gamma > 1.0, "gamma must be > 1");

    // Ring angles (ring, angle_rad) indexed 0..15
    let def: [(bool, f64); 16] = [
        (false, PI / 4.0),          // 0  outer
        (false, -PI / 4.0),         // 1
        (false, 3.0 * PI / 4.0),    // 2
        (false, -3.0 * PI / 4.0),   // 3
        (false, PI / 12.0),         // 4
        (false, -PI / 12.0),        // 5
        (false, 11.0 * PI / 12.0),  // 6
        (false, -11.0 * PI / 12.0), // 7
        (false, 5.0 * PI / 12.0),   // 8
        (false, -5.0 * PI / 12.0),  // 9
        (false, 7.0 * PI / 12.0),   // 10
        (false, -7.0 * PI / 12.0),  // 11
        (true, PI / 4.0),           // 12 inner
        (true, -PI / 4.0),          // 13
        (true, 3.0 * PI / 4.0),     // 14
        (true, -3.0 * PI / 4.0),    // 15
    ];

    let r1_raw = 1.0 / gamma;
    let r2_raw = 1.0;
    // Normalisation Es = 1: E = (4*r1^2 + 12*r2^2) / 16
    let r0 = (4.0 / (r1_raw * r1_raw + 3.0 * r2_raw * r2_raw)).sqrt();
    let r1 = r1_raw * r0;
    let r2 = r2_raw * r0;

    let mut points = Vec::with_capacity(16);
    for &(is_inner, angle) in &def {
        let r = if is_inner { r1 } else { r2 };
        points.push(Complex64::new(r * angle.cos(), r * angle.sin()));
    }

    let bit_map = make_bit_map(16, 4);
    Constellation {
        points,
        bits_per_sym: 4,
        bit_map,
    }
}

fn make_bit_map(n_points: usize, bits_per_sym: usize) -> Vec<Vec<u8>> {
    (0..n_points)
        .map(|i| {
            (0..bits_per_sym)
                .map(|k| ((i >> (bits_per_sym - 1 - k)) & 1) as u8)
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpsk_es_unity() {
        let c = qpsk_gray();
        let es: f64 = c.points.iter().map(|p| p.norm_sqr()).sum::<f64>() / c.points.len() as f64;
        assert!((es - 1.0).abs() < 1e-12);
    }

    #[test]
    fn psk8_es_unity() {
        let c = psk8_gray();
        let es: f64 = c.points.iter().map(|p| p.norm_sqr()).sum::<f64>() / c.points.len() as f64;
        assert!((es - 1.0).abs() < 1e-12);
    }

    #[test]
    fn apsk16_es_unity() {
        let c = apsk16_dvbs2(2.85);
        let es: f64 = c.points.iter().map(|p| p.norm_sqr()).sum::<f64>() / c.points.len() as f64;
        assert!((es - 1.0).abs() < 1e-10);
    }

    #[test]
    fn qpsk_roundtrip() {
        let c = qpsk_gray();
        let bits = vec![0, 0, 0, 1, 1, 1, 1, 0];
        let syms = c.map_bits(&bits);
        let idx = c.slice_nearest(&syms);
        let bits_out = c.symbols_to_bits(&idx);
        assert_eq!(bits, bits_out);
    }

    #[test]
    fn psk8_roundtrip() {
        let c = psk8_gray();
        let bits = vec![0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 1, 1, 1, 0, 0, 1, 0, 1, 1, 1, 0, 1, 1, 1];
        let syms = c.map_bits(&bits);
        let idx = c.slice_nearest(&syms);
        let bits_out = c.symbols_to_bits(&idx);
        assert_eq!(bits, bits_out);
    }

    #[test]
    fn apsk16_roundtrip() {
        let c = apsk16_dvbs2(2.85);
        // All 16 symbols
        let bits: Vec<u8> = (0..16u8)
            .flat_map(|i| (0..4).rev().map(move |k| (i >> k) & 1))
            .collect();
        let syms = c.map_bits(&bits);
        let idx = c.slice_nearest(&syms);
        let bits_out = c.symbols_to_bits(&idx);
        assert_eq!(bits, bits_out);
    }

    #[test]
    fn psk8_gray_neighbors_differ_one_bit() {
        let c = psk8_gray();
        // Adjacent angular points should differ by 1 bit
        for k in 0..8usize {
            let next = (k + 1) % 8;
            // Find which indices correspond to angle k*pi/4 and (k+1)*pi/4
            let idx_k = k ^ (k >> 1);
            let idx_next = next ^ (next >> 1);
            let diff: usize = (0..3)
                .filter(|&b| c.bit_map[idx_k][b] != c.bit_map[idx_next][b])
                .count();
            assert_eq!(diff, 1, "8PSK Gray: angular neighbors {k} and {next} differ by {diff} bits");
        }
    }
}
