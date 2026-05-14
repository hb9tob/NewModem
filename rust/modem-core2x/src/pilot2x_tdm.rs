//! TDM pilot symbols and interleaving for the 2x wire format.
//!
//! Port of V3 [`modem_core::pilot`] with one knob exposed (`d_syms`,
//! `p_syms` per profile via [`PilotPattern2x`]) so we can tune the
//! density and update-bandwidth axes independently without touching the
//! encoder/decoder. The pilot value is the **rotating QPSK sequence**
//! shared with V3 (phases `{0, π/2, π, 3π/2}` cycling on the absolute
//! pilot index) — that pattern is what lets V3 HIGH+ track FTX-1 + SDR
//! phase noise at σ² ≤ 0.02 on a sound-card link; the V4 sparse-block
//! variant (`pilot_block.rs`, deprecated by this module) drops a factor
//! ×8 of update bandwidth and could not match V3 on the same OTA capture
//! (see memory note `v4-pilot-tdm-refactor-todo`).
//!
//! Layout per LDPC codeword (post-interleave):
//!
//! ```text
//! [d_syms data] [p_syms pilots] [d_syms data] [p_syms pilots] ...
//! ```
//!
//! The last group is truncated to whatever data remains; pilots are
//! always inserted in full so the position table is deterministic.

use std::f64::consts::PI;

use modem_core_base::types::Complex64;

/// TDM pilot pattern: `p_syms` QPSK pilots inserted after every block
/// of `d_syms` data symbols. The values are derived from the absolute
/// pilot index `group_idx · p_syms + k` so TX and RX reconstruct the
/// same sequence given the same pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PilotPattern2x {
    pub d_syms: usize,
    pub p_syms: usize,
}

impl PilotPattern2x {
    /// 32 data + 2 pilot — the V3 HighPlus default. ~5.88 % overhead,
    /// pilot anchor every 34 sym (~22.7 ms at 1500 Bd).
    pub const fn default_2x() -> Self {
        Self { d_syms: 32, p_syms: 2 }
    }

    /// 16 data + 2 pilot — V3 dense_ultra / HighPlusPlus pattern.
    /// ~11.1 % overhead, anchor every 18 sym. Reserved for the
    /// experimental densified APSK-64 path.
    pub const fn dense_2x() -> Self {
        Self { d_syms: 16, p_syms: 2 }
    }

    /// Number of (data+pilot) groups required to cover `data_len`
    /// data symbols. The last group may be partial on the data side
    /// (still emits `p_syms` pilots).
    #[inline]
    pub fn n_groups(&self, data_len: usize) -> usize {
        (data_len + self.d_syms - 1) / self.d_syms
    }

    /// Total wire symbols emitted for a codeword of `data_len`
    /// data symbols under this pattern.
    #[inline]
    pub fn wire_len(&self, data_len: usize) -> usize {
        data_len + self.n_groups(data_len) * self.p_syms
    }
}

/// Pilot symbol #n: QPSK on the unit circle, phases `{0, π/2, π, 3π/2}`.
/// Bit-for-bit identical to V3 `modem_core::pilot::pilot_symbol`.
#[inline]
pub fn pilot_symbol_2x(n: usize) -> Complex64 {
    let phase = (n % 4) as f64 * PI / 2.0;
    Complex64::new(phase.cos(), phase.sin())
}

/// `pattern.p_syms` consecutive pilots for the `group_idx`-th TDM
/// group. The starting absolute pilot index is
/// `group_idx · pattern.p_syms` so the rotation continues across
/// groups (and across codewords if the caller threads a single counter
/// — frame2x does this by passing `group_idx_offset`).
pub fn pilots_for_group_2x(
    group_idx_abs: usize,
    pattern: &PilotPattern2x,
) -> Vec<Complex64> {
    (0..pattern.p_syms)
        .map(|k| pilot_symbol_2x(group_idx_abs * pattern.p_syms + k))
        .collect()
}

/// Interleave data symbols with TDM pilots, starting the pilot index
/// rotation at `group_idx_offset`. Returns
/// `(interleaved, pilot_positions)` where each position is a
/// half-open `(start, end)` range in the output buffer.
pub fn interleave_data_pilots_2x(
    data_syms: &[Complex64],
    pattern: &PilotPattern2x,
    group_idx_offset: usize,
) -> (Vec<Complex64>, Vec<(usize, usize)>) {
    let d = pattern.d_syms;
    let n_groups = pattern.n_groups(data_syms.len());
    let total_len = pattern.wire_len(data_syms.len());
    let mut out = Vec::with_capacity(total_len);
    let mut pilot_positions = Vec::with_capacity(n_groups);
    let mut cursor = 0;

    for g in 0..n_groups {
        let start = g * d;
        let end = (start + d).min(data_syms.len());
        out.extend_from_slice(&data_syms[start..end]);
        cursor += end - start;

        let pilots = pilots_for_group_2x(group_idx_offset + g, pattern);
        let p_start = cursor;
        out.extend_from_slice(&pilots);
        cursor += pilots.len();
        pilot_positions.push((p_start, cursor));
    }

    (out, pilot_positions)
}

/// Reverse of [`interleave_data_pilots_2x`]: walk the same group
/// layout and copy only the data slots into a fresh buffer.
/// `data_len` is required (the last group may be partial).
pub fn deinterleave_data_pilots_2x(
    interleaved: &[Complex64],
    pattern: &PilotPattern2x,
    data_len: usize,
) -> Vec<Complex64> {
    let expected = pattern.wire_len(data_len);
    assert_eq!(
        interleaved.len(),
        expected,
        "interleaved.len()={} expected {expected} (data_len={data_len}, \
         d={} p={})",
        interleaved.len(),
        pattern.d_syms,
        pattern.p_syms,
    );
    let d = pattern.d_syms;
    let p = pattern.p_syms;
    let n_groups = pattern.n_groups(data_len);
    let mut out = Vec::with_capacity(data_len);
    let mut wire_cursor = 0usize;
    let mut data_cursor = 0usize;
    for _ in 0..n_groups {
        let take = (d).min(data_len - data_cursor);
        out.extend_from_slice(&interleaved[wire_cursor..wire_cursor + take]);
        wire_cursor += take + p; // skip the pilot slot
        data_cursor += take;
    }
    out
}

/// Helper: returns the pilot-symbol position table for a CW of
/// `data_len` symbols, without materialising the interleaved buffer.
/// Used by the RX to walk the wire and pick the right pilot reference
/// for each captured sample. The table is `(p_start_in_wire, p_end,
/// abs_pilot_index_at_p_start)` per group.
pub fn pilot_positions_2x(
    data_len: usize,
    pattern: &PilotPattern2x,
    group_idx_offset: usize,
) -> Vec<(usize, usize, usize)> {
    let d = pattern.d_syms;
    let p = pattern.p_syms;
    let n_groups = pattern.n_groups(data_len);
    let mut out = Vec::with_capacity(n_groups);
    let mut wire_cursor = 0usize;
    let mut data_cursor = 0usize;
    for g in 0..n_groups {
        let take = (d).min(data_len - data_cursor);
        wire_cursor += take;
        let abs_pilot_start = (group_idx_offset + g) * p;
        out.push((wire_cursor, wire_cursor + p, abs_pilot_start));
        wire_cursor += p;
        data_cursor += take;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pilot_unit_circle() {
        for n in 0..20 {
            let s = pilot_symbol_2x(n);
            assert!((s.norm() - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn pilot_cycle_4() {
        let p0 = pilot_symbol_2x(0);
        let p4 = pilot_symbol_2x(4);
        assert!((p0 - p4).norm() < 1e-12);
    }

    #[test]
    fn pilot_bit_for_bit_v3_compatible() {
        // The V3 reference: phases 0, π/2, π, 3π/2 → (1,0), (0,1), (-1,0), (0,-1).
        let expect = [
            Complex64::new(1.0, 0.0),
            Complex64::new(0.0, 1.0),
            Complex64::new(-1.0, 0.0),
            Complex64::new(0.0, -1.0),
        ];
        for (i, e) in expect.iter().enumerate() {
            let got = pilot_symbol_2x(i);
            assert!((got - *e).norm() < 1e-12, "n={i} got={got:?} expect={e:?}");
        }
    }

    #[test]
    fn interleave_exact_groups() {
        let pattern = PilotPattern2x::default_2x();
        let data: Vec<Complex64> = (0..64)
            .map(|i| Complex64::new(i as f64, 0.0))
            .collect();
        let (out, positions) = interleave_data_pilots_2x(&data, &pattern, 0);
        assert_eq!(out.len(), 64 + 2 * pattern.p_syms);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0], (32, 34));
        assert_eq!(positions[1], (66, 68));
    }

    #[test]
    fn interleave_partial_group() {
        let pattern = PilotPattern2x::default_2x();
        let data: Vec<Complex64> = (0..40)
            .map(|i| Complex64::new(i as f64, 0.0))
            .collect();
        let (out, positions) = interleave_data_pilots_2x(&data, &pattern, 0);
        assert_eq!(out.len(), 40 + 2 * pattern.p_syms);
        assert_eq!(positions[0], (32, 34));
        // Last group: 8 data + 2 pilots at indices 42..44
        assert_eq!(positions[1], (42, 44));
    }

    #[test]
    fn interleave_dense_pattern() {
        let pattern = PilotPattern2x::dense_2x();
        let data: Vec<Complex64> = (0..32)
            .map(|i| Complex64::new(i as f64, 0.0))
            .collect();
        let (out, positions) = interleave_data_pilots_2x(&data, &pattern, 0);
        assert_eq!(out.len(), 32 + 2 * pattern.p_syms);
        assert_eq!(positions[0], (16, 18));
        assert_eq!(positions[1], (34, 36));
    }

    #[test]
    fn deinterleave_roundtrip() {
        let pattern = PilotPattern2x::default_2x();
        let data: Vec<Complex64> = (0..461)
            .map(|i| Complex64::new(i as f64, -(i as f64) * 0.1))
            .collect();
        let (out, _pos) = interleave_data_pilots_2x(&data, &pattern, 0);
        let recovered = deinterleave_data_pilots_2x(&out, &pattern, data.len());
        assert_eq!(recovered, data);
    }

    #[test]
    fn deinterleave_roundtrip_dense() {
        let pattern = PilotPattern2x::dense_2x();
        let data: Vec<Complex64> = (0..384)
            .map(|i| Complex64::new(i as f64, (i as f64) * 0.05))
            .collect();
        let (out, _pos) = interleave_data_pilots_2x(&data, &pattern, 0);
        let recovered = deinterleave_data_pilots_2x(&out, &pattern, data.len());
        assert_eq!(recovered, data);
    }

    #[test]
    fn group_offset_continues_pilot_rotation() {
        // Calling interleave on two CWs back-to-back with the right
        // offset must produce the same pilot sequence as a single
        // interleave on the concatenation (rotation is continuous).
        let pattern = PilotPattern2x::default_2x();
        let data_a: Vec<Complex64> = (0..64)
            .map(|i| Complex64::new(i as f64, 0.0))
            .collect();
        let data_b: Vec<Complex64> = (64..128)
            .map(|i| Complex64::new(i as f64, 0.0))
            .collect();
        // Two-step: encode A starting at group 0, then B starting at
        // group n_groups(A) = 2.
        let (mut out_ab, _) = interleave_data_pilots_2x(&data_a, &pattern, 0);
        let n_groups_a = pattern.n_groups(data_a.len());
        let (out_b, _) = interleave_data_pilots_2x(&data_b, &pattern, n_groups_a);
        out_ab.extend(out_b);
        // Reference single-step.
        let data_full: Vec<Complex64> = data_a.iter().chain(data_b.iter()).copied().collect();
        let (out_ref, _) = interleave_data_pilots_2x(&data_full, &pattern, 0);
        assert_eq!(out_ab, out_ref);
    }

    #[test]
    fn pilot_positions_table_matches_interleave() {
        let pattern = PilotPattern2x::default_2x();
        let data: Vec<Complex64> = (0..200)
            .map(|i| Complex64::new(i as f64, 0.0))
            .collect();
        let (out, _) = interleave_data_pilots_2x(&data, &pattern, 7);
        let table = pilot_positions_2x(data.len(), &pattern, 7);
        for &(s, e, abs_idx) in &table {
            for k in s..e {
                let want = pilot_symbol_2x(abs_idx + (k - s));
                assert!(
                    (out[k] - want).norm() < 1e-12,
                    "mismatch at k={k} abs_idx={abs_idx}",
                );
            }
        }
    }

    #[test]
    fn wire_len_matches_interleave_output() {
        let pattern = PilotPattern2x::default_2x();
        for &n in &[1usize, 31, 32, 33, 64, 65, 100, 461, 1152] {
            let data: Vec<Complex64> = (0..n)
                .map(|i| Complex64::new(i as f64, 0.0))
                .collect();
            let (out, _) = interleave_data_pilots_2x(&data, &pattern, 0);
            assert_eq!(out.len(), pattern.wire_len(n), "n={n}");
        }
    }
}
