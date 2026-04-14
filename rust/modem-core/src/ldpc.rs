//! Encoder LDPC WiMAX (IEEE 802.16). Codes proven, exportes depuis commpy.
//!
//! - Rate 3/4 : (960, 720)
//! - Rate 1/2 : (1440, 720)
//!
//! Format de fichier binaire (genere par export_ldpc.py) :
//!   u32 LE k, u32 LE n, puis (n-k) lignes de (k+7)/8 octets (MSB-first)
//!
//! Encodage systematique : codeword = [info | parity], avec
//!   parity[i] = XOR_j (P[i][j] AND info[j])

const WIMAX_960_720: &[u8] = include_bytes!("ldpc_data/wimax_960_720.bin");
const WIMAX_1440_720: &[u8] = include_bytes!("ldpc_data/wimax_1440_720.bin");

#[derive(Debug, Clone, Copy)]
pub enum LdpcCode {
    /// WiMAX 802.16 rate 3/4, (960, 720)
    Rate34,
    /// WiMAX 802.16 rate 1/2, (1440, 720)
    Rate12,
}

impl LdpcCode {
    pub fn k(&self) -> usize { 720 }
    pub fn n(&self) -> usize {
        match self { LdpcCode::Rate34 => 960, LdpcCode::Rate12 => 1440 }
    }
    pub fn data(&self) -> &'static [u8] {
        match self {
            LdpcCode::Rate34 => WIMAX_960_720,
            LdpcCode::Rate12 => WIMAX_1440_720,
        }
    }
    pub fn rate_num(&self) -> usize { match self { LdpcCode::Rate34 => 3, LdpcCode::Rate12 => 1 } }
    pub fn rate_den(&self) -> usize { match self { LdpcCode::Rate34 => 4, LdpcCode::Rate12 => 2 } }
}

/// Charge la matrice parity P depuis les octets compresses.
/// Retourne (k, n, P) ou P[i][j] est le bit (i,j) de la matrice.
fn load_parity_matrix(code: LdpcCode) -> (usize, usize, Vec<Vec<u8>>) {
    let data = code.data();
    let k = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let n = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    assert_eq!(k, code.k());
    assert_eq!(n, code.n());
    let bytes_per_row = (k + 7) / 8;
    let n_rows = n - k;
    let mut p = vec![vec![0u8; k]; n_rows];
    let mut off = 8;
    for i in 0..n_rows {
        for j in 0..k {
            let b = data[off + j / 8];
            p[i][j] = (b >> (7 - (j % 8))) & 1;
        }
        off += bytes_per_row;
    }
    (k, n, p)
}

/// LDPC encoder systematique. Encode des blocs de k bits en codewords de n bits.
pub struct LdpcEncoder {
    pub code: LdpcCode,
    parity_matrix: Vec<Vec<u8>>,
}

impl LdpcEncoder {
    pub fn new(code: LdpcCode) -> Self {
        let (_, _, p) = load_parity_matrix(code);
        LdpcEncoder { code, parity_matrix: p }
    }

    pub fn k(&self) -> usize { self.code.k() }
    pub fn n(&self) -> usize { self.code.n() }

    /// Encode un bloc d'info (k bits) en codeword (n bits).
    pub fn encode_block(&self, info: &[u8]) -> Vec<u8> {
        assert_eq!(info.len(), self.k(), "info length must be k={}", self.k());
        let mut cw = Vec::with_capacity(self.n());
        cw.extend_from_slice(info);
        for row in &self.parity_matrix {
            let mut acc: u8 = 0;
            for j in 0..self.k() {
                acc ^= row[j] & info[j];
            }
            cw.push(acc);
        }
        cw
    }

    /// Encode plusieurs blocs concatenes. info.len() doit etre multiple de k.
    /// Padd avec des zeros si necessaire.
    pub fn encode_padded(&self, info: &[u8]) -> Vec<u8> {
        let k = self.k();
        let extra = (k - info.len() % k) % k;
        let mut padded = info.to_vec();
        padded.extend(std::iter::repeat(0u8).take(extra));
        let n_blocks = padded.len() / k;
        let mut cw_total = Vec::with_capacity(n_blocks * self.n());
        for b in 0..n_blocks {
            let block = &padded[b * k..(b + 1) * k];
            cw_total.extend_from_slice(&self.encode_block(block));
        }
        cw_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldpc_rate_34_dimensions() {
        let enc = LdpcEncoder::new(LdpcCode::Rate34);
        assert_eq!(enc.k(), 720);
        assert_eq!(enc.n(), 960);
        let info = vec![0u8; 720];
        let cw = enc.encode_block(&info);
        assert_eq!(cw.len(), 960);
        // Info 0 -> parity 0
        assert!(cw.iter().all(|&b| b == 0));
    }

    #[test]
    fn ldpc_rate_12_dimensions() {
        let enc = LdpcEncoder::new(LdpcCode::Rate12);
        assert_eq!(enc.k(), 720);
        assert_eq!(enc.n(), 1440);
    }

    #[test]
    fn ldpc_systematic_first_k_bits() {
        let enc = LdpcEncoder::new(LdpcCode::Rate34);
        let mut info = vec![0u8; 720];
        for i in 0..720 {
            info[i] = ((i * 17 + 3) % 2) as u8;
        }
        let cw = enc.encode_block(&info);
        assert_eq!(&cw[..720], &info[..]);
    }

    #[test]
    fn ldpc_padded_roundtrip_length() {
        let enc = LdpcEncoder::new(LdpcCode::Rate34);
        let info = vec![1u8; 800];  // Pas multiple de 720
        let cw = enc.encode_padded(&info);
        // 800 -> padd a 1440 (2 blocs de 720) -> 2 cw de 960 = 1920
        assert_eq!(cw.len(), 1920);
    }
}
