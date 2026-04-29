//! Constellations: QPSK Gray, 8PSK Gray, 16-APSK DVB-S2 (4,12), 32-APSK DVB-S2 (4,12,16),
//! 64-APSK DVB-S2X (4,12,20,28).
//!
//! 16-APSK : port de modem_apsk16_ftn_bench.py lignes 30-245.
//! 32-APSK : ETSI EN 302 307-1 §5.4.4 Figure 12, table reprise de
//! gr-dvbs2 (drmpeg) modulator_bc_impl.cc lignes 171-202.
//! 64-APSK : ETSI EN 302 307-2 V1.4.1 §5.4.5 Tables 13e (mapping) et
//! 13f (rayons) — pas de port SDR de référence (gr-dvbs2/rx/acm
//! n'implémentent pas ce layout, vérifié 2026-04).
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

/// 32-APSK (4, 12, 16) DVB-S2, normalised Es = 1.
///
/// `gamma1 = R2 / R1`, `gamma2 = R3 / R1`. Table 10 EN 302 307-1 :
/// rate 3/4 → γ1=2.84, γ2=5.27 ; rate 5/6 → γ1=2.64, γ2=4.64.
///
/// Bit-label → point mapping per Figure 12 of EN 302 307-1, transcribed
/// from the gr-dvbs2 reference implementation (m_32apsk array, indices
/// 0..31 = 5-bit binary labels).
pub fn apsk32_dvbs2(gamma1: f64, gamma2: f64) -> Constellation {
    assert!(gamma1 > 1.0 && gamma2 > gamma1, "expected 1 < gamma1 < gamma2");

    // Per Figure 12: ring designator R1 (inner, 4 pts), R2 (middle, 12 pts),
    // R3 (outer, 16 pts). Index = 5-bit label (binary, MSB first).
    #[derive(Clone, Copy)]
    enum Ring {
        R1,
        R2,
        R3,
    }
    use Ring::*;
    const PI: f64 = std::f64::consts::PI;
    let def: [(Ring, f64); 32] = [
        (R2, PI / 4.0),         //  0  00000
        (R2, 5.0 * PI / 12.0),  //  1  00001
        (R2, -PI / 4.0),        //  2  00010
        (R2, -5.0 * PI / 12.0), //  3  00011
        (R2, 3.0 * PI / 4.0),   //  4  00100
        (R2, 7.0 * PI / 12.0),  //  5  00101
        (R2, -3.0 * PI / 4.0),  //  6  00110
        (R2, -7.0 * PI / 12.0), //  7  00111
        (R3, PI / 8.0),         //  8  01000
        (R3, 3.0 * PI / 8.0),   //  9  01001
        (R3, -PI / 4.0),        // 10  01010
        (R3, -PI / 2.0),        // 11  01011
        (R3, 3.0 * PI / 4.0),   // 12  01100
        (R3, PI / 2.0),         // 13  01101
        (R3, -7.0 * PI / 8.0),  // 14  01110
        (R3, -5.0 * PI / 8.0),  // 15  01111
        (R2, PI / 12.0),        // 16  10000
        (R1, PI / 4.0),         // 17  10001
        (R2, -PI / 12.0),       // 18  10010
        (R1, -PI / 4.0),        // 19  10011
        (R2, 11.0 * PI / 12.0), // 20  10100
        (R1, 3.0 * PI / 4.0),   // 21  10101
        (R2, -11.0 * PI / 12.0),// 22  10110
        (R1, -3.0 * PI / 4.0),  // 23  10111
        (R3, 0.0),              // 24  11000
        (R3, PI / 4.0),         // 25  11001
        (R3, -PI / 8.0),        // 26  11010
        (R3, -3.0 * PI / 8.0),  // 27  11011
        (R3, 7.0 * PI / 8.0),   // 28  11100
        (R3, 5.0 * PI / 8.0),   // 29  11101
        (R3, PI),               // 30  11110
        (R3, -3.0 * PI / 4.0),  // 31  11111
    ];

    // Raw radii with R1=1, then normalise so Es = 1.
    // Es = (4*R1² + 12*R2² + 16*R3²) / 32 = (R1² + 3*R2² + 4*R3²) / 8.
    let r1_raw = 1.0;
    let r2_raw = gamma1;
    let r3_raw = gamma2;
    let r0 = (8.0 / (r1_raw * r1_raw + 3.0 * r2_raw * r2_raw + 4.0 * r3_raw * r3_raw)).sqrt();
    let r1 = r1_raw * r0;
    let r2 = r2_raw * r0;
    let r3 = r3_raw * r0;

    let mut points = Vec::with_capacity(32);
    for &(ring, angle) in &def {
        let r = match ring {
            R1 => r1,
            R2 => r2,
            R3 => r3,
        };
        points.push(Complex64::new(r * angle.cos(), r * angle.sin()));
    }

    let bit_map = make_bit_map(32, 5);
    Constellation {
        points,
        bits_per_sym: 5,
        bit_map,
    }
}

/// 64-APSK (4+12+20+28) DVB-S2X, normalised Es = 1.
///
/// `gamma1 = R2/R1`, `gamma2 = R3/R1`, `gamma3 = R4/R1`. La norme
/// EN 302 307-2 V1.4.1 Table 13f publie un seul jeu de rayons pour ce
/// layout, pour LDPC 11/15 (≈0.733) : γ1=2.4, γ2=4.3, γ3=7.0. Pas de
/// jeu officiel pour LDPC 3/4 dans ce layout — voir profile_high_plus_plus.
///
/// Bit-label → point mapping per Table 13e of EN 302 307-2 V1.4.1.
/// Aucun port SDR de référence (gr-dvbs2/rx/acm n'implémentent pas
/// ce layout, vérifié 2026-04). Angles donnés en fractions de π,
/// transcrits littéralement de la table normative.
pub fn apsk64_dvbs2x(gamma1: f64, gamma2: f64, gamma3: f64) -> Constellation {
    assert!(
        gamma1 > 1.0 && gamma2 > gamma1 && gamma3 > gamma2,
        "expected 1 < gamma1 < gamma2 < gamma3",
    );

    #[derive(Clone, Copy)]
    enum Ring {
        R1,
        R2,
        R3,
        R4,
    }
    use Ring::*;

    // Chaque entrée : (anneau, num, den) — angle = num · π / den.
    // Transcription directe d'EN 302 307-2 Table 13e ; index = label
    // 6 bits binaire MSB-first (« xxxxpq »).
    const DEF: [(Ring, u32, u32); 64] = [
        (R4,  1,  4), //  0  000000
        (R4,  7,  4), //  1  000001
        (R4,  3,  4), //  2  000010
        (R4,  5,  4), //  3  000011
        (R4, 13, 28), //  4  000100
        (R4, 43, 28), //  5  000101
        (R4, 15, 28), //  6  000110
        (R4, 41, 28), //  7  000111
        (R4,  1, 28), //  8  001000
        (R4, 55, 28), //  9  001001
        (R4, 27, 28), // 10  001010
        (R4, 29, 28), // 11  001011
        (R1,  1,  4), // 12  001100
        (R1,  7,  4), // 13  001101
        (R1,  3,  4), // 14  001110
        (R1,  5,  4), // 15  001111
        (R4,  9, 28), // 16  010000
        (R4, 47, 28), // 17  010001
        (R4, 19, 28), // 18  010010
        (R4, 37, 28), // 19  010011
        (R4, 11, 28), // 20  010100
        (R4, 45, 28), // 21  010101
        (R4, 17, 28), // 22  010110
        (R4, 39, 28), // 23  010111
        (R3,  1, 20), // 24  011000
        (R3, 39, 20), // 25  011001
        (R3, 19, 20), // 26  011010
        (R3, 21, 20), // 27  011011
        (R2,  1, 12), // 28  011100
        (R2, 23, 12), // 29  011101
        (R2, 11, 12), // 30  011110
        (R2, 13, 12), // 31  011111
        (R4,  5, 28), // 32  100000
        (R4, 51, 28), // 33  100001
        (R4, 23, 28), // 34  100010
        (R4, 33, 28), // 35  100011
        (R3,  9, 20), // 36  100100
        (R3, 31, 20), // 37  100101
        (R3, 11, 20), // 38  100110
        (R3, 29, 20), // 39  100111
        (R4,  3, 28), // 40  101000
        (R4, 53, 28), // 41  101001
        (R4, 25, 28), // 42  101010
        (R4, 31, 28), // 43  101011
        (R2,  5, 12), // 44  101100
        (R2, 19, 12), // 45  101101
        (R2,  7, 12), // 46  101110
        (R2, 17, 12), // 47  101111
        (R3,  1,  4), // 48  110000
        (R3,  7,  4), // 49  110001
        (R3,  3,  4), // 50  110010
        (R3,  5,  4), // 51  110011
        (R3,  7, 20), // 52  110100
        (R3, 33, 20), // 53  110101
        (R3, 13, 20), // 54  110110
        (R3, 27, 20), // 55  110111
        (R3,  3, 20), // 56  111000
        (R3, 37, 20), // 57  111001
        (R3, 17, 20), // 58  111010
        (R3, 23, 20), // 59  111011
        (R2,  1,  4), // 60  111100
        (R2,  7,  4), // 61  111101
        (R2,  3,  4), // 62  111110
        (R2,  5,  4), // 63  111111
    ];

    // Rayons bruts avec R1=1, puis normalisation Es=1.
    // Es = (4·R1² + 12·R2² + 20·R3² + 28·R4²) / 64
    //    = (R1² + 3·R2² + 5·R3² + 7·R4²) / 16
    let r1_raw = 1.0_f64;
    let r2_raw = gamma1;
    let r3_raw = gamma2;
    let r4_raw = gamma3;
    let r0 = (16.0
        / (r1_raw * r1_raw
            + 3.0 * r2_raw * r2_raw
            + 5.0 * r3_raw * r3_raw
            + 7.0 * r4_raw * r4_raw))
        .sqrt();
    let r1 = r1_raw * r0;
    let r2 = r2_raw * r0;
    let r3 = r3_raw * r0;
    let r4 = r4_raw * r0;

    let mut points = Vec::with_capacity(64);
    for &(ring, num, den) in &DEF {
        let r = match ring {
            R1 => r1,
            R2 => r2,
            R3 => r3,
            R4 => r4,
        };
        let angle = num as f64 * PI / den as f64;
        points.push(Complex64::new(r * angle.cos(), r * angle.sin()));
    }

    let bit_map = make_bit_map(64, 6);
    Constellation {
        points,
        bits_per_sym: 6,
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
    fn apsk32_es_unity() {
        let c = apsk32_dvbs2(2.84, 5.27);
        assert_eq!(c.points.len(), 32);
        assert_eq!(c.bits_per_sym, 5);
        let es: f64 = c.points.iter().map(|p| p.norm_sqr()).sum::<f64>() / c.points.len() as f64;
        assert!((es - 1.0).abs() < 1e-10, "Es = {es}");
    }

    #[test]
    fn apsk32_three_rings() {
        // Vérifie qu'on a bien 4 points sur R1, 12 sur R2, 16 sur R3.
        let c = apsk32_dvbs2(2.84, 5.27);
        let mut radii: Vec<f64> = c.points.iter().map(|p| p.norm()).collect();
        radii.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // Plus petits 4 = R1, suivants 12 = R2, derniers 16 = R3.
        let r1 = radii[0];
        let r2 = radii[4];
        let r3 = radii[16];
        // Tous les points de chaque anneau ont le même rayon.
        for k in 0..4 {
            assert!((radii[k] - r1).abs() < 1e-10, "R1 #{k} radius mismatch");
        }
        for k in 4..16 {
            assert!((radii[k] - r2).abs() < 1e-10, "R2 #{k} radius mismatch");
        }
        for k in 16..32 {
            assert!((radii[k] - r3).abs() < 1e-10, "R3 #{k} radius mismatch");
        }
        // Ratios DVB-S2 rate 3/4.
        assert!((r2 / r1 - 2.84).abs() < 1e-10, "γ1 mismatch: {}", r2 / r1);
        assert!((r3 / r1 - 5.27).abs() < 1e-10, "γ2 mismatch: {}", r3 / r1);
    }

    #[test]
    fn apsk32_dvbs2_reference_points() {
        // Vecteurs de référence : pour rate 3/4 (γ1=2.84, γ2=5.27),
        // après normalisation Es=1, R1=r0, R2=2.84·r0, R3=5.27·r0
        // avec r0 = sqrt(8/(1 + 3·2.84² + 4·5.27²)).
        let c = apsk32_dvbs2(2.84, 5.27);
        let r0 = (8.0_f64 / (1.0 + 3.0 * 2.84 * 2.84 + 4.0 * 5.27 * 5.27)).sqrt();
        let r1 = r0;
        let r2 = 2.84 * r0;
        let r3 = 5.27 * r0;
        let pi = std::f64::consts::PI;

        // Index 17 (10001) → R1 à π/4.
        let p17 = c.points[0b10001];
        let exp17 = Complex64::new(r1 * (pi / 4.0).cos(), r1 * (pi / 4.0).sin());
        assert!((p17 - exp17).norm() < 1e-10);

        // Index 0 (00000) → R2 à π/4.
        let p0 = c.points[0b00000];
        let exp0 = Complex64::new(r2 * (pi / 4.0).cos(), r2 * (pi / 4.0).sin());
        assert!((p0 - exp0).norm() < 1e-10);

        // Index 24 (11000) → R3 à 0.
        let p24 = c.points[0b11000];
        let exp24 = Complex64::new(r3, 0.0);
        assert!((p24 - exp24).norm() < 1e-10);

        // Index 13 (01101) → R3 à π/2.
        let p13 = c.points[0b01101];
        let exp13 = Complex64::new(0.0, r3);
        assert!((p13 - exp13).norm() < 1e-10);
    }

    #[test]
    fn apsk32_distinct_points() {
        let c = apsk32_dvbs2(2.84, 5.27);
        for i in 0..32 {
            for j in (i + 1)..32 {
                let d = (c.points[i] - c.points[j]).norm();
                assert!(d > 1e-6, "points {i} and {j} are identical");
            }
        }
    }

    #[test]
    fn apsk32_roundtrip() {
        let c = apsk32_dvbs2(2.84, 5.27);
        let bits: Vec<u8> = (0..32u8)
            .flat_map(|i| (0..5).rev().map(move |k| (i >> k) & 1))
            .collect();
        let syms = c.map_bits(&bits);
        let idx = c.slice_nearest(&syms);
        let bits_out = c.symbols_to_bits(&idx);
        assert_eq!(bits, bits_out);
    }

    #[test]
    fn apsk64_es_unity() {
        let c = apsk64_dvbs2x(2.4, 4.3, 7.0);
        assert_eq!(c.points.len(), 64);
        assert_eq!(c.bits_per_sym, 6);
        let es: f64 = c.points.iter().map(|p| p.norm_sqr()).sum::<f64>() / c.points.len() as f64;
        assert!((es - 1.0).abs() < 1e-10, "Es = {es}");
    }

    #[test]
    fn apsk64_four_rings() {
        // Vérifie qu'on a bien 4 points sur R1, 12 sur R2, 20 sur R3, 28 sur R4.
        let c = apsk64_dvbs2x(2.4, 4.3, 7.0);
        let mut radii: Vec<f64> = c.points.iter().map(|p| p.norm()).collect();
        radii.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let r1 = radii[0];
        let r2 = radii[4];
        let r3 = radii[16];
        let r4 = radii[36];
        for k in 0..4 {
            assert!((radii[k] - r1).abs() < 1e-10, "R1 #{k} radius mismatch");
        }
        for k in 4..16 {
            assert!((radii[k] - r2).abs() < 1e-10, "R2 #{k} radius mismatch");
        }
        for k in 16..36 {
            assert!((radii[k] - r3).abs() < 1e-10, "R3 #{k} radius mismatch");
        }
        for k in 36..64 {
            assert!((radii[k] - r4).abs() < 1e-10, "R4 #{k} radius mismatch");
        }
        // Ratios DVB-S2X 4+12+20+28 (Table 13f, MODCOD 64APSK 11/15).
        assert!((r2 / r1 - 2.4).abs() < 1e-10, "γ1 mismatch: {}", r2 / r1);
        assert!((r3 / r1 - 4.3).abs() < 1e-10, "γ2 mismatch: {}", r3 / r1);
        assert!((r4 / r1 - 7.0).abs() < 1e-10, "γ3 mismatch: {}", r4 / r1);
    }

    #[test]
    fn apsk64_dvbs2x_reference_points() {
        // Vecteurs de référence : pour rate 11/15 (γ1=2.4, γ2=4.3, γ3=7.0),
        // après normalisation Es=1, R1=r0, R2=2.4·r0, R3=4.3·r0, R4=7.0·r0
        // avec r0 = sqrt(16 / (1 + 3·2.4² + 5·4.3² + 7·7.0²)).
        let c = apsk64_dvbs2x(2.4, 4.3, 7.0);
        let r0 = (16.0_f64 / (1.0 + 3.0 * 2.4 * 2.4 + 5.0 * 4.3 * 4.3 + 7.0 * 7.0 * 7.0)).sqrt();
        let r1 = r0;
        let r2 = 2.4 * r0;
        let r3 = 4.3 * r0;
        let r4 = 7.0 * r0;
        let pi = std::f64::consts::PI;

        // Index 12 (001100) → R1 à π/4.
        let p12 = c.points[0b001100];
        let exp12 = Complex64::new(r1 * (pi / 4.0).cos(), r1 * (pi / 4.0).sin());
        assert!((p12 - exp12).norm() < 1e-10, "idx 12 R1 π/4");

        // Index 28 (011100) → R2 à π/12.
        let p28 = c.points[0b011100];
        let exp28 = Complex64::new(r2 * (pi / 12.0).cos(), r2 * (pi / 12.0).sin());
        assert!((p28 - exp28).norm() < 1e-10, "idx 28 R2 π/12");

        // Index 24 (011000) → R3 à π/20.
        let p24 = c.points[0b011000];
        let exp24 = Complex64::new(r3 * (pi / 20.0).cos(), r3 * (pi / 20.0).sin());
        assert!((p24 - exp24).norm() < 1e-10, "idx 24 R3 π/20");

        // Index 0 (000000) → R4 à π/4.
        let p0 = c.points[0b000000];
        let exp0 = Complex64::new(r4 * (pi / 4.0).cos(), r4 * (pi / 4.0).sin());
        assert!((p0 - exp0).norm() < 1e-10, "idx 0 R4 π/4");

        // Index 8 (001000) → R4 à π/28 (rayon le plus serré sur R4, idx canonique en haut).
        let p8 = c.points[0b001000];
        let exp8 = Complex64::new(r4 * (pi / 28.0).cos(), r4 * (pi / 28.0).sin());
        assert!((p8 - exp8).norm() < 1e-10, "idx 8 R4 π/28");
    }

    #[test]
    fn apsk64_distinct_points() {
        let c = apsk64_dvbs2x(2.4, 4.3, 7.0);
        for i in 0..64 {
            for j in (i + 1)..64 {
                let d = (c.points[i] - c.points[j]).norm();
                assert!(d > 1e-6, "points {i} and {j} are identical");
            }
        }
    }

    #[test]
    fn apsk64_roundtrip() {
        let c = apsk64_dvbs2x(2.4, 4.3, 7.0);
        let bits: Vec<u8> = (0..64u8)
            .flat_map(|i| (0..6).rev().map(move |k| (i >> k) & 1))
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
