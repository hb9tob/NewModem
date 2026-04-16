//! WiMAX IEEE 802.16e LDPC base matrices.
//!
//! Base matrices from IEEE 802.16e-2005:
//! - Rate 1/2: Table 574 (12 x 24)
//! - Rate 2/3A: Table 575a (8 x 24)
//! - Rate 3/4A: Table 576a (6 x 24)
//!
//! Expansion factor z = 96, giving N = 24 * 96 = 2304.
//! Entry -1 means zero z×z block. Entry s (0..z-1) means
//! z×z identity matrix cyclically shifted right by s positions.

use crate::profile::LdpcRate;

/// A base matrix entry: -1 = zero block, 0..95 = cyclic shift of I_z.
pub type BaseEntry = i16;

/// Base matrix for a given rate.
pub struct BaseMatrix {
    pub n_rows: usize,  // m_b (check nodes)
    pub n_cols: usize,  // n_b (always 24)
    pub data: &'static [BaseEntry],
}

/// Expansion factor.
pub const Z: usize = 96;
/// Codeword length.
pub const N: usize = 24 * Z; // 2304

impl BaseMatrix {
    pub fn get(&self, row: usize, col: usize) -> BaseEntry {
        self.data[row * self.n_cols + col]
    }
}

/// Get the base matrix for a given LDPC rate.
pub fn base_matrix(rate: LdpcRate) -> BaseMatrix {
    match rate {
        LdpcRate::R1_2 => BaseMatrix {
            n_rows: 12,
            n_cols: 24,
            data: &BASE_R1_2,
        },
        LdpcRate::R2_3 => BaseMatrix {
            n_rows: 8,
            n_cols: 24,
            data: &BASE_R2_3,
        },
        LdpcRate::R3_4 => BaseMatrix {
            n_rows: 6,
            n_cols: 24,
            data: &BASE_R3_4,
        },
    }
}

/// Sparse representation of the expanded H matrix.
/// For each check node (row), list of (column_index, _) pairs.
pub struct SparseH {
    pub n_rows: usize,
    pub n_cols: usize,
    /// For each row: sorted list of column indices where H[row][col] = 1.
    pub row_indices: Vec<Vec<usize>>,
    /// For each column: sorted list of row indices where H[row][col] = 1.
    pub col_indices: Vec<Vec<usize>>,
}

/// Expand a base matrix into a full sparse H matrix.
pub fn expand(bm: &BaseMatrix) -> SparseH {
    let n_rows = bm.n_rows * Z;
    let n_cols = bm.n_cols * Z;
    let mut row_indices = vec![Vec::new(); n_rows];
    let mut col_indices = vec![Vec::new(); n_cols];

    for br in 0..bm.n_rows {
        for bc in 0..bm.n_cols {
            let shift = bm.get(br, bc);
            if shift < 0 {
                continue;
            }
            let s = shift as usize;
            // This base entry expands to a z×z identity shifted right by s.
            // Row i of the sub-block has a 1 at column (i + s) mod z.
            for i in 0..Z {
                let row = br * Z + i;
                let col = bc * Z + (i + s) % Z;
                row_indices[row].push(col);
                col_indices[col].push(row);
            }
        }
    }

    // Sort for deterministic ordering
    for r in &mut row_indices {
        r.sort_unstable();
    }
    for c in &mut col_indices {
        c.sort_unstable();
    }

    SparseH {
        n_rows,
        n_cols,
        row_indices,
        col_indices,
    }
}

// ===========================================================================
// Base matrices from IEEE 802.16e-2005
// ===========================================================================

/// Rate 1/2. 12 rows × 24 columns, z=96.
/// Source: RPTU Channel Codes Database (IEEE 802.16e-2005 Table 574).
/// Verified: H rank = 1152 (full), all entries in [-1, 95].
#[rustfmt::skip]
const BASE_R1_2: [BaseEntry; 12 * 24] = [
    -1, 94, 73, -1, -1, -1, -1, -1, 55, 83, -1, -1,  7,  0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
    -1, 27, -1, -1, -1, 22, 79,  9, -1, -1, -1, 12, -1,  0,  0, -1, -1, -1, -1, -1, -1, -1, -1, -1,
    -1, -1, -1, 24, 22, 81, -1, 33, -1, -1, -1,  0, -1, -1,  0,  0, -1, -1, -1, -1, -1, -1, -1, -1,
    61, -1, 47, -1, -1, -1, -1, -1, 65, 25, -1, -1, -1, -1, -1,  0,  0, -1, -1, -1, -1, -1, -1, -1,
    -1, -1, 39, -1, -1, -1, 84, -1, -1, 41, 72, -1, -1, -1, -1, -1,  0,  0, -1, -1, -1, -1, -1, -1,
    -1, -1, -1, -1, 46, 40, -1, 82, -1, -1, -1, 79,  0, -1, -1, -1, -1,  0,  0, -1, -1, -1, -1, -1,
    -1, -1, 95, 53, -1, -1, -1, -1, -1, 14, 18, -1, -1, -1, -1, -1, -1, -1,  0,  0, -1, -1, -1, -1,
    -1, 11, 73, -1, -1, -1,  2, -1, -1, 47, -1, -1, -1, -1, -1, -1, -1, -1, -1,  0,  0, -1, -1, -1,
    12, -1, -1, -1, 83, 24, -1, 43, -1, -1, -1, 51, -1, -1, -1, -1, -1, -1, -1, -1,  0,  0, -1, -1,
    -1, -1, -1, -1, -1, 94, -1, 59, -1, -1, 70, 72, -1, -1, -1, -1, -1, -1, -1, -1, -1,  0,  0, -1,
    -1, -1,  7, 65, -1, -1, -1, -1, 39, 49, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,  0,  0,
    43, -1, -1, -1, -1, 66, -1, 41, -1, -1, -1, 26,  7, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,  0,
];

/// Rate 2/3B. 8 rows × 24 columns, z=96.
/// Source: RPTU Channel Codes Database (IEEE 802.16e-2005).
/// Verified: H rank = 768 (full), all entries in [-1, 95].
#[rustfmt::skip]
const BASE_R2_3: [BaseEntry; 8 * 24] = [
     2, -1, 19, -1, 47, -1, 48, -1, 36, -1, 82, -1, 47, -1, 15, -1, 95,  0, -1, -1, -1, -1, -1, -1,
    -1, 69, -1, 88, -1, 33, -1,  3, -1, 16, -1, 37, -1, 40, -1, 48, -1,  0,  0, -1, -1, -1, -1, -1,
    10, -1, 86, -1, 62, -1, 28, -1, 85, -1, 16, -1, 34, -1, 73, -1, -1, -1,  0,  0, -1, -1, -1, -1,
    -1, 28, -1, 32, -1, 81, -1, 27, -1, 88, -1,  5, -1, 56, -1, 37, -1, -1, -1,  0,  0, -1, -1, -1,
    23, -1, 29, -1, 15, -1, 30, -1, 66, -1, 24, -1, 50, -1, 62, -1, -1, -1, -1, -1,  0,  0, -1, -1,
    -1, 30, -1, 65, -1, 54, -1, 14, -1,  0, -1, 30, -1, 74, -1,  0, -1, -1, -1, -1, -1,  0,  0, -1,
    32, -1,  0, -1, 15, -1, 56, -1, 85, -1,  5, -1,  6, -1, 52, -1,  0, -1, -1, -1, -1, -1,  0,  0,
    -1,  0, -1, 47, -1, 13, -1, 61, -1, 84, -1, 55, -1, 78, -1, 41, 95, -1, -1, -1, -1, -1, -1,  0,
];

/// Rate 3/4A. 6 rows × 24 columns, z=96.
/// Source: RPTU Channel Codes Database (IEEE 802.16e-2005).
/// Verified: H rank = 576 (full), all entries in [-1, 95].
#[rustfmt::skip]
const BASE_R3_4: [BaseEntry; 6 * 24] = [
     6, 38,  3, 93, -1, -1, -1, 30, 70, -1, 86, -1, 37, 38,  4, 11, -1, 46, 48,  0, -1, -1, -1, -1,
    62, 94, 19, 84, -1, 92, 78, -1, 15, -1, -1, 92, -1, 45, 24, 32, 30, -1, -1,  0,  0, -1, -1, -1,
    71, -1, 55, -1, 12, 66, 45, 79, -1, 78, -1, -1, 10, -1, 22, 55, 70, 82, -1, -1,  0,  0, -1, -1,
    38, 61, -1, 66,  9, 73, 47, 64, -1, 39, 61, 43, -1, -1, -1, -1, 95, 32,  0, -1, -1,  0,  0, -1,
    -1, -1, -1, -1, 32, 52, 55, 80, 95, 22,  6, 51, 24, 90, 44, 20, -1, -1, -1, -1, -1, -1,  0,  0,
    -1, 63, 31, 88, 20, -1, -1, -1,  6, 40, 56, 16, 71, 53, -1, -1, 27, 26, 48, -1, -1, -1, -1,  0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_matrix_dimensions() {
        let bm = base_matrix(LdpcRate::R1_2);
        assert_eq!(bm.n_rows, 12);
        assert_eq!(bm.n_cols, 24);
        assert_eq!(bm.data.len(), 12 * 24);

        let bm = base_matrix(LdpcRate::R2_3);
        assert_eq!(bm.n_rows, 8);
        assert_eq!(bm.n_cols, 24);

        let bm = base_matrix(LdpcRate::R3_4);
        assert_eq!(bm.n_rows, 6);
        assert_eq!(bm.n_cols, 24);
    }

    #[test]
    fn expand_r1_2_dimensions() {
        let bm = base_matrix(LdpcRate::R1_2);
        let h = expand(&bm);
        assert_eq!(h.n_rows, 12 * 96);
        assert_eq!(h.n_cols, 24 * 96);
    }

    #[test]
    fn expand_row_degree() {
        // Each row should have degree = number of non-(-1) entries in that base row
        let bm = base_matrix(LdpcRate::R1_2);
        let h = expand(&bm);
        for br in 0..bm.n_rows {
            let expected_degree: usize = (0..bm.n_cols)
                .filter(|&bc| bm.get(br, bc) >= 0)
                .count();
            // Each expanded row within this base row should have exactly `expected_degree` ones
            for i in 0..Z {
                let row = br * Z + i;
                assert_eq!(
                    h.row_indices[row].len(),
                    expected_degree,
                    "Row {row} (base row {br}) has wrong degree"
                );
            }
        }
    }

    #[test]
    fn expand_symmetry() {
        // H[r][c] = 1 iff c is in row_indices[r] iff r is in col_indices[c]
        let bm = base_matrix(LdpcRate::R1_2);
        let h = expand(&bm);
        for r in 0..h.n_rows {
            for &c in &h.row_indices[r] {
                assert!(
                    h.col_indices[c].contains(&r),
                    "H[{r}][{c}]=1 but col_indices[{c}] doesn't contain {r}"
                );
            }
        }
    }

    #[test]
    fn shift_values_in_range() {
        for rate in [LdpcRate::R1_2, LdpcRate::R2_3, LdpcRate::R3_4] {
            let bm = base_matrix(rate);
            for r in 0..bm.n_rows {
                for c in 0..bm.n_cols {
                    let v = bm.get(r, c);
                    assert!(v >= -1 && v < Z as i16, "Out of range: {v} at ({r},{c})");
                }
            }
        }
    }
}
