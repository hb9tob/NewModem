//! LDPC Layered Normalized Min-Sum (LNMS) decoder.
//!
//! Standard iterative decoder for WiMAX IEEE 802.16e codes.
//! LNMS converges ~2x faster than flooding BP, avoids tanh computation.
//! Normalization factor alpha = 0.75 (standard for WiMAX codes).
//! ~0.2 dB loss vs full BP, acceptable for our SNR margins.

use crate::profile::LdpcRate;
use super::wimax::{self, GroupedH, SparseH};
use pulp::{Arch, Simd, WithSimd};

/// LNMS normalization factor.
const ALPHA: f32 = 0.75;

/// LLR clipping to prevent numerical overflow.
const LLR_CLIP: f32 = 25.0;

pub struct LdpcDecoder {
    rate: LdpcRate,
    h: SparseH,
    grouped: GroupedH,
    k: usize,
    n: usize,
    m: usize,
    max_iter: usize,
    /// Stop iterating early on an UNDECODABLE block when the syndrome weight
    /// (number of unsatisfied checks) stops decreasing — the documented
    /// failure-case early-termination (UC Davis ISCAS 2011). The success stop
    /// (weight == 0) is always on; this adds the failure stop. Default ON
    /// (validated byte-identical on a real OTA capture); kill-switch
    /// `V3_NO_LDPC_FAIL_STOP` restores the full 50-iteration budget.
    fail_stop: bool,
    /// Ignore the first `fail_warmup` iterations before arming the failure stop
    /// (min-sum can dip then recover early on). `V3_LDPC_FAIL_WARMUP`.
    fail_warmup: usize,
    /// Declare a block undecodable after this many consecutive non-improving
    /// iterations past the warmup. `V3_LDPC_FAIL_STAG`.
    fail_stag: usize,
}

impl LdpcDecoder {
    pub fn new(rate: LdpcRate, max_iter: usize) -> Self {
        let bm = wimax::base_matrix(rate);
        let h = wimax::expand(&bm);
        let grouped = wimax::group(&bm);
        let k = rate.k();
        let n = rate.n();
        let m = n - k;
        let env_usize = |k: &str, d: usize| {
            std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
        };
        LdpcDecoder {
            rate,
            h,
            grouped,
            k,
            n,
            m,
            max_iter,
            fail_stop: std::env::var_os("V3_NO_LDPC_FAIL_STOP").is_none(),
            fail_warmup: env_usize("V3_LDPC_FAIL_WARMUP", 5),
            fail_stag: env_usize("V3_LDPC_FAIL_STAG", 3).max(1),
        }
    }

    /// Decode LLR values (length = n = 2304) into info bits.
    ///
    /// LLR convention: positive = bit 0 more likely.
    /// Returns (decoded_info_bits, converged).
    pub fn decode(&self, llr_channel: &[f32]) -> (Vec<u8>, bool) {
        assert_eq!(llr_channel.len(), self.n);

        // Q messages: variable-to-check. Indexed as [row][position_in_row].
        // R messages: check-to-variable. Same indexing.
        // We store R messages in a flat structure aligned with row_indices.

        // Precompute: for each row, the column indices and their positions
        let mut r_messages: Vec<Vec<f32>> = self.h.row_indices.iter()
            .map(|cols| vec![0.0f32; cols.len()])
            .collect();

        // Total LLR for each variable node (channel + sum of incoming check messages)
        let mut total_llr: Vec<f32> = llr_channel.iter()
            .map(|&x| x.clamp(-LLR_CLIP, LLR_CLIP))
            .collect();

        let mut converged = false;

        for _iter in 0..self.max_iter {
            // Layered schedule: process each check node row
            for row in 0..self.m {
                let cols = &self.h.row_indices[row];
                let degree = cols.len();

                // Step 1: Compute Q messages (variable-to-check)
                // Q[row][j] = total_llr[cols[j]] - R[row][j]  (extrinsic)
                let q_msgs: Vec<f32> = (0..degree)
                    .map(|j| total_llr[cols[j]] - r_messages[row][j])
                    .collect();

                // Step 2: Min-Sum check node update
                // For each output j:
                //   R_new[row][j] = alpha * sign_product * second_min_excluding_j
                // where sign_product is the product of signs of all Q except j,
                // and min_excluding_j is the minimum |Q| excluding position j.

                // Find the two smallest |Q| values and their positions
                let mut min1_val = f32::INFINITY;
                let mut min1_idx = 0usize;
                let mut min2_val = f32::INFINITY;
                let mut sign_product: i8 = 1;

                for (j, &q) in q_msgs.iter().enumerate() {
                    let absq = q.abs();
                    if q < 0.0 {
                        sign_product = -sign_product;
                    }
                    if absq < min1_val {
                        min2_val = min1_val;
                        min1_val = absq;
                        min1_idx = j;
                    } else if absq < min2_val {
                        min2_val = absq;
                    }
                }

                // Step 3: Compute new R messages and update total_llr
                for j in 0..degree {
                    let old_r = r_messages[row][j];

                    // Sign: product of all signs except j
                    let sign_j = if q_msgs[j] < 0.0 { -sign_product } else { sign_product };

                    // Magnitude: minimum of |Q| excluding j
                    let mag = if j == min1_idx { min2_val } else { min1_val };

                    let new_r = ALPHA * sign_j as f32 * mag;
                    let new_r = new_r.clamp(-LLR_CLIP, LLR_CLIP);

                    // Update total LLR
                    total_llr[cols[j]] += new_r - old_r;
                    r_messages[row][j] = new_r;
                }
            }

            // Check convergence: all parity checks satisfied?
            if self.check_syndrome(&total_llr) {
                converged = true;
                break;
            }
        }

        // Hard decision on info bits
        let info_bits: Vec<u8> = total_llr[..self.k]
            .iter()
            .map(|&l| if l < 0.0 { 1 } else { 0 })
            .collect();

        (info_bits, converged)
    }

    /// Check if all parity checks are satisfied (hard decision). Cheap success
    /// test (early-returns on the first unsatisfied row).
    fn check_syndrome(&self, total_llr: &[f32]) -> bool {
        for row in 0..self.m {
            let mut parity = 0u8;
            for &col in &self.h.row_indices[row] {
                if total_llr[col] < 0.0 {
                    parity ^= 1;
                }
            }
            if parity != 0 {
                return false;
            }
        }
        true
    }

    /// Number of UNSATISFIED parity checks (the syndrome weight). `0` ⇔
    /// `check_syndrome` true (converged). Unlike `check_syndrome` this scans all
    /// rows (no early return) so the turbo loop can watch the weight fall — a
    /// weight that stops decreasing flags an undecodable block (failure stop).
    fn syndrome_weight(&self, total_llr: &[f32]) -> usize {
        let mut weight = 0usize;
        for row in 0..self.m {
            let mut parity = 0u8;
            for &col in &self.h.row_indices[row] {
                if total_llr[col] < 0.0 {
                    parity ^= 1;
                }
            }
            weight += parity as usize;
        }
        weight
    }

    /// Decode LLR and return info bytes. Convenience wrapper.
    pub fn decode_to_bytes(&self, llr: &[f32]) -> (Vec<u8>, bool) {
        let (bits, converged) = self.decode(llr);
        let bytes: Vec<u8> = bits.chunks(8)
            .map(|chunk| {
                let mut byte = 0u8;
                for (i, &b) in chunk.iter().enumerate() {
                    byte |= (b & 1) << (7 - i);
                }
                byte
            })
            .collect();
        (bytes, converged)
    }

    /// SISO decode for turbo equalization (Tüchler/Koetter/Singer 2002).
    ///
    /// Mirrors [`decode`] exactly (same LNMS schedule) but seeds the variable
    /// nodes with `channel_llr + apriori_llr`, and returns the per-coded-bit
    /// EXTRINSIC over the full `n` — the decoder's NEW information for the
    /// equalizer = posterior − seeded prior = Σ (check-to-variable messages),
    /// scaled by `extrinsic_damp ∈ (0,1]` to relax the min-sum overconfidence
    /// and keep the turbo loop from diverging. Subtracting the seeded prior
    /// (channel AND a-priori) is mandatory: returning the posterior would feed
    /// the equalizer's own information back to it (positive feedback).
    ///
    /// LLR convention: positive = bit 0 more likely (same as [`decode`]).
    /// `apriori_llr = None` on the first turbo iteration (all-zero prior).
    /// Returns `(decoded_info_bits, extrinsic_llr_full_n, converged)`.
    pub fn decode_soft(
        &self,
        channel_llr: &[f32],
        apriori_llr: Option<&[f32]>,
        extrinsic_damp: f32,
    ) -> (Vec<u8>, Vec<f32>, bool) {
        assert_eq!(channel_llr.len(), self.n);
        if let Some(a) = apriori_llr {
            assert_eq!(a.len(), self.n);
        }

        let mut r_messages: Vec<Vec<f32>> = self.h.row_indices.iter()
            .map(|cols| vec![0.0f32; cols.len()])
            .collect();

        // Seed = (channel + a-priori), clamped. Keep the CLAMPED seed so the
        // extrinsic is exactly Σ R (posterior − seed), with no double-counting.
        let seed: Vec<f32> = (0..self.n)
            .map(|i| {
                let ap = apriori_llr.map_or(0.0, |a| a[i]);
                (channel_llr[i] + ap).clamp(-LLR_CLIP, LLR_CLIP)
            })
            .collect();
        let mut total_llr: Vec<f32> = seed.clone();

        let mut converged = false;
        let mut prev_weight = usize::MAX;
        let mut stagnant = 0usize;
        for iter in 0..self.max_iter {
            for row in 0..self.m {
                let cols = &self.h.row_indices[row];
                let degree = cols.len();
                let q_msgs: Vec<f32> = (0..degree)
                    .map(|j| total_llr[cols[j]] - r_messages[row][j])
                    .collect();
                let mut min1_val = f32::INFINITY;
                let mut min1_idx = 0usize;
                let mut min2_val = f32::INFINITY;
                let mut sign_product: i8 = 1;
                for (j, &q) in q_msgs.iter().enumerate() {
                    let absq = q.abs();
                    if q < 0.0 {
                        sign_product = -sign_product;
                    }
                    if absq < min1_val {
                        min2_val = min1_val;
                        min1_val = absq;
                        min1_idx = j;
                    } else if absq < min2_val {
                        min2_val = absq;
                    }
                }
                for j in 0..degree {
                    let old_r = r_messages[row][j];
                    let sign_j = if q_msgs[j] < 0.0 { -sign_product } else { sign_product };
                    let mag = if j == min1_idx { min2_val } else { min1_val };
                    let new_r = (ALPHA * sign_j as f32 * mag).clamp(-LLR_CLIP, LLR_CLIP);
                    total_llr[cols[j]] += new_r - old_r;
                    r_messages[row][j] = new_r;
                }
            }
            let weight = self.syndrome_weight(&total_llr);
            if weight == 0 {
                converged = true;
                break;
            }
            // Failure stop (opt-in V3_LDPC_FAIL_STOP): the syndrome weight has
            // stopped improving on an undecodable block — further iterations
            // only burn cycles, so bail (returns the stagnation-point extrinsic).
            if self.fail_stop && iter + 1 > self.fail_warmup {
                if weight >= prev_weight {
                    stagnant += 1;
                    if stagnant >= self.fail_stag {
                        break;
                    }
                } else {
                    stagnant = 0;
                }
            }
            prev_weight = weight;
        }

        let info_bits: Vec<u8> = total_llr[..self.k]
            .iter()
            .map(|&l| if l < 0.0 { 1 } else { 0 })
            .collect();
        let extrinsic: Vec<f32> = (0..self.n)
            .map(|i| (extrinsic_damp * (total_llr[i] - seed[i])).clamp(-LLR_CLIP, LLR_CLIP))
            .collect();
        (info_bits, extrinsic, converged)
    }

    /// SIMD-friendly grouped twin of [`decode_soft`], producing **bit-identical**
    /// output (info bits, extrinsic, converged flag) — verified by
    /// `grouped_decode_is_bit_exact_vs_scalar`. It walks the QC base matrix
    /// layer by layer and processes the `z` circulant rows of each layer across
    /// lanes (here a scalar `for lane`; the pulp SIMD kernel will replace only
    /// the lane loops). Within a layer the `z` rows touch *disjoint* variables,
    /// and every channel read (pass A) happens before any write (pass C), so the
    /// lane-parallel order is indistinguishable from the scalar per-row sweep
    /// (see [`super::wimax::GroupedH`]).
    ///
    /// The grouped layout also reuses flat scratch buffers across iterations
    /// instead of `decode_soft`'s per-call `Vec<Vec<f32>>` — already most of the
    /// speed-up before any vectorisation. `decode_soft` is left byte-identical so
    /// the OTA-validated path is untouched until this twin is validated and wired.
    pub fn decode_soft_grouped(
        &self,
        channel_llr: &[f32],
        apriori_llr: Option<&[f32]>,
        extrinsic_damp: f32,
    ) -> (Vec<u8>, Vec<f32>, bool) {
        assert_eq!(channel_llr.len(), self.n);
        if let Some(a) = apriori_llr {
            assert_eq!(a.len(), self.n);
        }
        let z = self.grouped.z;
        let g = &self.grouped;

        // Seed = (channel + a-priori), clamped — identical to `decode_soft`.
        let seed: Vec<f32> = (0..self.n)
            .map(|i| {
                let ap = apriori_llr.map_or(0.0, |a| a[i]);
                (channel_llr[i] + ap).clamp(-LLR_CLIP, LLR_CLIP)
            })
            .collect();
        let mut total_llr: Vec<f32> = seed.clone();

        // Flat check-to-variable messages: r_msgs[layer_offset + edge*z + lane].
        let mut r_msgs: Vec<f32> = vec![0.0f32; g.total_edge_lanes];

        // Reusable per-layer scratch (sized to the widest layer).
        let max_deg = g.layers.iter().map(|e| e.len()).max().unwrap_or(0);
        let mut q_buf: Vec<f32> = vec![0.0f32; max_deg * z];
        let mut min1 = vec![0.0f32; z];
        let mut min2 = vec![0.0f32; z];
        let mut min1_idx = vec![0usize; z];
        let mut sign = vec![1.0f32; z];

        let mut converged = false;
        for _iter in 0..self.max_iter {
            for (br, edges) in g.layers.iter().enumerate() {
                let d = edges.len();
                let base = g.layer_offsets[br];
                // Pass A: q[e,lane] = total[col(e,lane)] - r[e,lane].
                for (e, &(bc, s)) in edges.iter().enumerate() {
                    let qo = e * z;
                    let ro = base + e * z;
                    let cb = bc * z;
                    for lane in 0..z {
                        let col = cb + (lane + s) % z;
                        q_buf[qo + lane] = total_llr[col] - r_msgs[ro + lane];
                    }
                }
                // Pass B: per-lane min1 / min2 / argmin / sign over the d edges
                // (strict `<`, first-minimum index — exactly the scalar order).
                for lane in 0..z {
                    let mut m1 = f32::INFINITY;
                    let mut m2 = f32::INFINITY;
                    let mut idx = 0usize;
                    let mut sg = 1.0f32;
                    for e in 0..d {
                        let q = q_buf[e * z + lane];
                        let a = q.abs();
                        if q < 0.0 {
                            sg = -sg;
                        }
                        if a < m1 {
                            m2 = m1;
                            m1 = a;
                            idx = e;
                        } else if a < m2 {
                            m2 = a;
                        }
                    }
                    min1[lane] = m1;
                    min2[lane] = m2;
                    min1_idx[lane] = idx;
                    sign[lane] = sg;
                }
                // Pass C: write new r + update total (disjoint columns per lane).
                for (e, &(bc, s)) in edges.iter().enumerate() {
                    let qo = e * z;
                    let ro = base + e * z;
                    let cb = bc * z;
                    for lane in 0..z {
                        let q = q_buf[qo + lane];
                        let sign_j = if q < 0.0 { -sign[lane] } else { sign[lane] };
                        let mag = if e == min1_idx[lane] { min2[lane] } else { min1[lane] };
                        let new_r = (ALPHA * sign_j * mag).clamp(-LLR_CLIP, LLR_CLIP);
                        let col = cb + (lane + s) % z;
                        total_llr[col] += new_r - r_msgs[ro + lane];
                        r_msgs[ro + lane] = new_r;
                    }
                }
            }
            if self.check_syndrome(&total_llr) {
                converged = true;
                break;
            }
        }

        let info_bits: Vec<u8> = total_llr[..self.k]
            .iter()
            .map(|&l| if l < 0.0 { 1 } else { 0 })
            .collect();
        let extrinsic: Vec<f32> = (0..self.n)
            .map(|i| (extrinsic_damp * (total_llr[i] - seed[i])).clamp(-LLR_CLIP, LLR_CLIP))
            .collect();
        (info_bits, extrinsic, converged)
    }

    /// pulp-vectorised twin of [`decode_soft_grouped`] — same grouped layout and
    /// the same per-lane f32 operations, but the `z`-wide lane loops run on AVX2
    /// (x86_64) / NEON (aarch64) via runtime dispatch, falling back to scalar
    /// where neither is present. **Bit-identical** to `decode_soft` (verified by
    /// `simd_decode_is_bit_exact_vs_scalar`): every lane replicates the scalar
    /// min1/min2/argmin/sign reduction with the same operation order and no
    /// cross-lane reduction, and the circulant shift is materialised by two
    /// contiguous copies, so no result-changing reorder occurs.
    pub fn decode_soft_simd(
        &self,
        channel_llr: &[f32],
        apriori_llr: Option<&[f32]>,
        extrinsic_damp: f32,
    ) -> (Vec<u8>, Vec<f32>, bool) {
        assert_eq!(channel_llr.len(), self.n);
        if let Some(a) = apriori_llr {
            assert_eq!(a.len(), self.n);
        }
        let z = self.grouped.z;
        let seed: Vec<f32> = (0..self.n)
            .map(|i| {
                let ap = apriori_llr.map_or(0.0, |a| a[i]);
                (channel_llr[i] + ap).clamp(-LLR_CLIP, LLR_CLIP)
            })
            .collect();
        let mut total_llr = seed.clone();
        let mut r_msgs = vec![0.0f32; self.grouped.total_edge_lanes];
        let max_deg = self.grouped.layers.iter().map(|e| e.len()).max().unwrap_or(0);
        let mut q_buf = vec![0.0f32; max_deg * z];
        let mut rot = vec![0.0f32; z];
        let mut rot_delta = vec![0.0f32; z];

        let converged = Arch::new().dispatch(SoftKernel {
            dec: self,
            total: &mut total_llr,
            r_msgs: &mut r_msgs,
            q_buf: &mut q_buf,
            rot: &mut rot,
            rot_delta: &mut rot_delta,
        });

        let info_bits: Vec<u8> = total_llr[..self.k]
            .iter()
            .map(|&l| if l < 0.0 { 1 } else { 0 })
            .collect();
        let extrinsic: Vec<f32> = (0..self.n)
            .map(|i| (extrinsic_damp * (total_llr[i] - seed[i])).clamp(-LLR_CLIP, LLR_CLIP))
            .collect();
        (info_bits, extrinsic, converged)
    }

    pub fn k(&self) -> usize { self.k }
    pub fn n(&self) -> usize { self.n }
}

/// SIMD kernel backing [`LdpcDecoder::decode_soft_simd`]. Runs the whole
/// layered loop inside one pulp dispatch (amortising feature detection); each
/// lane reproduces the scalar per-row min-sum exactly — see the method doc.
struct SoftKernel<'a> {
    dec: &'a LdpcDecoder,
    total: &'a mut [f32],
    r_msgs: &'a mut [f32],
    q_buf: &'a mut [f32],
    rot: &'a mut [f32],
    rot_delta: &'a mut [f32],
}

impl WithSimd for SoftKernel<'_> {
    type Output = bool;

    #[inline(always)]
    fn with_simd<S: Simd>(self, simd: S) -> bool {
        let SoftKernel { dec, total, r_msgs, q_buf, rot, rot_delta } = self;
        let g = &dec.grouped;
        let z = g.z;
        let lanes = core::mem::size_of::<S::f32s>() / core::mem::size_of::<f32>();
        debug_assert_eq!(z % lanes, 0, "z must be a multiple of the SIMD lane count");
        let nchunks = z / lanes;

        let inf = simd.splat_f32s(f32::INFINITY);
        let zero = simd.splat_f32s(0.0);
        let one = simd.splat_f32s(1.0);
        let alpha = simd.splat_f32s(ALPHA);
        let lo = simd.splat_f32s(-LLR_CLIP);
        let hi = simd.splat_f32s(LLR_CLIP);

        // Per-lane reduction state, one SIMD register per z-chunk.
        let mut min1 = vec![inf; nchunks];
        let mut min2 = vec![inf; nchunks];
        let mut idxf = vec![zero; nchunks];
        let mut sign = vec![one; nchunks];

        let mut converged = false;
        let mut prev_weight = usize::MAX;
        let mut stagnant = 0usize;
        for iter in 0..dec.max_iter {
            for (br, edges) in g.layers.iter().enumerate() {
                let base = g.layer_offsets[br];
                let d = edges.len();

                // PASS A: q = rotate(total_block, s) - r, per edge.
                for (e, &(bc, s)) in edges.iter().enumerate() {
                    let cb = bc * z;
                    rot[..z - s].copy_from_slice(&total[cb + s..cb + z]);
                    rot[z - s..].copy_from_slice(&total[cb..cb + s]);
                    let (rotv, _) = S::as_simd_f32s(&rot[..]);
                    let (rv, _) = S::as_simd_f32s(&r_msgs[base + e * z..base + e * z + z]);
                    let (qv, _) = S::as_mut_simd_f32s(&mut q_buf[e * z..e * z + z]);
                    for k in 0..nchunks {
                        qv[k] = simd.sub_f32s(rotv[k], rv[k]);
                    }
                }

                // PASS B: per-lane min1 / min2 / argmin / sign over the d edges
                // (strict `<`, first-minimum index — exactly the scalar order).
                for k in 0..nchunks {
                    min1[k] = inf;
                    min2[k] = inf;
                    idxf[k] = zero;
                    sign[k] = one;
                }
                for e in 0..d {
                    let ef = simd.splat_f32s(e as f32);
                    let (qv, _) = S::as_simd_f32s(&q_buf[e * z..e * z + z]);
                    for k in 0..nchunks {
                        let q = qv[k];
                        let a = simd.abs_f32s(q);
                        let m1lt = simd.less_than_f32s(a, min1[k]);
                        let m2lt = simd.less_than_f32s(a, min2[k]);
                        // new_min2 = m1lt ? old_min1 : (m2lt ? a : old_min2)
                        let inner = simd.select_f32s_m32s(m2lt, a, min2[k]);
                        min2[k] = simd.select_f32s_m32s(m1lt, min1[k], inner);
                        min1[k] = simd.select_f32s_m32s(m1lt, a, min1[k]);
                        idxf[k] = simd.select_f32s_m32s(m1lt, ef, idxf[k]);
                        let qneg = simd.less_than_f32s(q, zero);
                        sign[k] =
                            simd.select_f32s_m32s(qneg, simd.sub_f32s(zero, sign[k]), sign[k]);
                    }
                }

                // PASS C: new r + scatter delta into total (disjoint per lane).
                for (e, &(bc, s)) in edges.iter().enumerate() {
                    let ef = simd.splat_f32s(e as f32);
                    {
                        let (qv, _) = S::as_simd_f32s(&q_buf[e * z..e * z + z]);
                        let (rv, _) =
                            S::as_mut_simd_f32s(&mut r_msgs[base + e * z..base + e * z + z]);
                        let (rdv, _) = S::as_mut_simd_f32s(&mut rot_delta[..]);
                        for k in 0..nchunks {
                            let q = qv[k];
                            let isarg = simd.equal_f32s(idxf[k], ef);
                            let mag = simd.select_f32s_m32s(isarg, min2[k], min1[k]);
                            let qneg = simd.less_than_f32s(q, zero);
                            let sj = simd
                                .select_f32s_m32s(qneg, simd.sub_f32s(zero, sign[k]), sign[k]);
                            // (ALPHA * sign_j) * mag, then clamp — same assoc/order
                            // and same strict compares as the scalar `f32::clamp`.
                            let mut nr = simd.mul_f32s(simd.mul_f32s(alpha, sj), mag);
                            let above = simd.greater_than_f32s(nr, hi);
                            nr = simd.select_f32s_m32s(above, hi, nr);
                            let below = simd.less_than_f32s(nr, lo);
                            nr = simd.select_f32s_m32s(below, lo, nr);
                            rdv[k] = simd.sub_f32s(nr, rv[k]);
                            rv[k] = nr;
                        }
                    }
                    let cb = bc * z;
                    for i in 0..z - s {
                        total[cb + s + i] += rot_delta[i];
                    }
                    for j in 0..s {
                        total[cb + j] += rot_delta[z - s + j];
                    }
                }
            }
            let weight = dec.syndrome_weight(total);
            if weight == 0 {
                converged = true;
                break;
            }
            if dec.fail_stop && iter + 1 > dec.fail_warmup {
                if weight >= prev_weight {
                    stagnant += 1;
                    if stagnant >= dec.fail_stag {
                        break;
                    }
                } else {
                    stagnant = 0;
                }
            }
            prev_weight = weight;
        }
        converged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ldpc::encoder::LdpcEncoder;

    fn encode_decode_test(rate: LdpcRate, noise_scale: f32) {
        let enc = LdpcEncoder::new(rate);
        let dec = LdpcDecoder::new(rate, 50);

        // Random info bits
        let mut info = vec![0u8; enc.k()];
        let mut state: u32 = 0xDEAD;
        for b in &mut info {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            *b = ((state >> 16) & 1) as u8;
        }

        let cw = enc.encode(&info);

        // Convert to LLR: bit=0 -> +LLR, bit=1 -> -LLR
        // Add small noise to LLR (simulates channel)
        let mut llr: Vec<f32> = cw.iter().enumerate().map(|(i, &b)| {
            let sign = if b == 0 { 1.0f32 } else { -1.0 };
            // Pseudo-random noise
            let mut s = (i as u32).wrapping_mul(2654435761).wrapping_add(state);
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            let noise = ((s >> 16) as f32 / 32768.0 - 1.0) * noise_scale;
            (sign * 4.0 + noise).clamp(-LLR_CLIP, LLR_CLIP)
        }).collect();

        let (decoded, converged) = dec.decode(&llr);
        assert!(converged, "Decoder did not converge for rate {:?}", rate);
        assert_eq!(decoded, info, "Decoded bits mismatch for rate {:?}", rate);
    }

    #[test]
    fn encode_decode_r1_2_clean() {
        encode_decode_test(LdpcRate::R1_2, 0.0);
    }

    #[test]
    fn encode_decode_r1_2_noisy() {
        encode_decode_test(LdpcRate::R1_2, 0.5);
    }

    #[test]
    fn encode_decode_r2_3_clean() {
        encode_decode_test(LdpcRate::R2_3, 0.0);
    }

    #[test]
    fn encode_decode_r3_4_clean() {
        encode_decode_test(LdpcRate::R3_4, 0.0);
    }

    #[test]
    fn encode_decode_r5_6_clean() {
        encode_decode_test(LdpcRate::R5_6, 0.0);
    }

    #[test]
    fn decode_all_zeros() {
        let dec = LdpcDecoder::new(LdpcRate::R1_2, 50);
        // All-zero codeword with positive LLR
        let llr = vec![5.0f32; 2304];
        let (decoded, converged) = dec.decode(&llr);
        assert!(converged);
        assert!(decoded.iter().all(|&b| b == 0));
    }

    /// Pseudo-random LLR vector in roughly `(-scale, scale)`, clamped.
    fn rand_llr(seed: u32, n: usize, scale: f32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                let u = ((s >> 16) as f32 / 32768.0) - 1.0; // ~U(-1,1)
                (u * scale).clamp(-LLR_CLIP, LLR_CLIP)
            })
            .collect()
    }

    /// The grouped/SIMD-ready decoder must be **bit-for-bit** identical to the
    /// OTA-validated scalar `decode_soft` — info bits, the full extrinsic vector
    /// (compared on raw bit patterns), and the converged flag — across every
    /// rate, at strong / marginal / pure-noise inputs (the last never converges,
    /// so it exercises the whole 50-iteration min-tracking path), with and
    /// without an a-priori. This is the gate for the layout change before pulp.
    fn assert_grouped_matches(rate: LdpcRate, scale: f32, with_apriori: bool, seed: u32) {
        let mut dec = LdpcDecoder::new(rate, 50);
        // Isolate the vectorisation bit-exactness from the failure early-stop:
        // `decode_soft_grouped` is the pure-layout reference WITHOUT the stop, so
        // compare all three with the stop OFF (the stop's scalar/SIMD parity is
        // covered by `simd_failstop_matches_scalar`).
        dec.fail_stop = false;
        let n = dec.n();
        let channel = rand_llr(seed, n, scale);
        let apri = if with_apriori {
            Some(rand_llr(seed ^ 0xABCD, n, 2.0))
        } else {
            None
        };
        let (b0, e0, c0) = dec.decode_soft(&channel, apri.as_deref(), 0.7);
        // Both the scalar-grouped layout and the pulp SIMD kernel must match the
        // OTA-validated scalar `decode_soft` bit-for-bit.
        for (tag, (b, e, c)) in [
            ("grouped", dec.decode_soft_grouped(&channel, apri.as_deref(), 0.7)),
            ("simd", dec.decode_soft_simd(&channel, apri.as_deref(), 0.7)),
        ] {
            assert_eq!(c0, c, "[{tag}] converged differs rate={rate:?} scale={scale} apri={with_apriori}");
            assert_eq!(b0, b, "[{tag}] info bits differ rate={rate:?} scale={scale} apri={with_apriori}");
            assert_eq!(e0.len(), e.len());
            for (i, (x, y)) in e0.iter().zip(e.iter()).enumerate() {
                assert_eq!(
                    x.to_bits(),
                    y.to_bits(),
                    "[{tag}] extrinsic[{i}] differs rate={rate:?} scale={scale} apri={with_apriori}: {x} vs {y}",
                );
            }
        }
    }

    #[test]
    fn grouped_decode_is_bit_exact_vs_scalar() {
        for &rate in &[LdpcRate::R1_2, LdpcRate::R2_3, LdpcRate::R3_4, LdpcRate::R5_6] {
            for &scale in &[6.0f32, 1.5, 0.3] {
                assert_grouped_matches(rate, scale, false, 0x1234_5678);
                assert_grouped_matches(rate, scale, true, 0x9E37_79B9);
            }
        }
    }

    #[test]
    fn simd_failstop_matches_scalar() {
        // With the failure early-stop ON (the default), the SIMD path must still
        // match the scalar `decode_soft` bit-for-bit: identical syndrome weights
        // each iteration → identical stagnation → identical stop iteration and
        // returned extrinsic. Marginal + pure-noise inputs exercise the stop.
        for &rate in &[LdpcRate::R1_2, LdpcRate::R2_3, LdpcRate::R3_4, LdpcRate::R5_6] {
            for &scale in &[1.5f32, 0.3] {
                let mut dec = LdpcDecoder::new(rate, 50);
                dec.fail_stop = true;
                let n = dec.n();
                let ch = rand_llr(0x5151_3737, n, scale);
                let (b0, e0, c0) = dec.decode_soft(&ch, None, 0.7);
                let (b1, e1, c1) = dec.decode_soft_simd(&ch, None, 0.7);
                assert_eq!(c0, c1, "fail_stop converged differs rate={rate:?} scale={scale}");
                assert_eq!(b0, b1, "fail_stop info bits differ rate={rate:?} scale={scale}");
                for (i, (x, y)) in e0.iter().zip(e1.iter()).enumerate() {
                    assert_eq!(
                        x.to_bits(),
                        y.to_bits(),
                        "fail_stop extrinsic[{i}] differs rate={rate:?} scale={scale}: {x} vs {y}",
                    );
                }
            }
        }
    }
}
