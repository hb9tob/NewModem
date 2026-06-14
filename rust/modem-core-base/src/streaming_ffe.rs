//! Stateful streaming **fractionally-spaced** FFE (T/`pitch_fse`) with
//! on-demand tap retraining.
//!
//! The block ingests the matched-filtered stream **decimated to T/2**
//! (`pitch_fse = 2` samples per symbol for tau = 1; the `StreamingDsp`
//! upstream emits it). It exposes a **symbol-spaced** interface: each
//! [`out_buf`](StreamingFfe::out_buf) entry is one equalised complex
//! sample per symbol, and [`raw_buf`](StreamingFfe::raw_buf) is the
//! on-symbol (un-equalised) decimation of the fractional input. Both are
//! indexed by absolute symbol from [`start_abs`](StreamingFfe::start_abs),
//! so every downstream consumer (marker probe, segment slicer) is
//! unchanged from the symbol-spaced version.
//!
//! **Why fractional** (the MER win): a symbol-spaced (T) FFE samples the
//! matched filter once per symbol at a fixed phase and cannot correct a
//! sub-symbol timing offset — the residual ISI capped streaming MER at
//! ~12 dB versus ~22 dB for the batch `rx_v2` path. The batch equaliser
//! is fractional (T/2) and LS-trained on the preamble; reproducing that
//! here lets the FFE place its taps across both half-symbol phases and
//! absorb the timing offset, so the META AppHeader CW and the data CWs
//! converge on real captures.
//!
//! The on-symbol subsample of the T/2 stream is byte-identical to the old
//! symbol-spaced decimation (`fse[s*pitch_fse] == mf[s*sps]`), so
//! `raw_buf` — what acquisition correlates against — does not change.
//!
//! **Forward-apply invariant** (carried over from the symbol-spaced
//! design, commit `1f73202`): every [`push_raw`](StreamingFfe::push_raw)
//! re-equalises the previous push's tail symbols, which had to be emitted
//! with a boundary fallback because they lacked right-hand FIR context.
//! Cycle N's marker/PLS decode then runs on a stream already cleaned by
//! cycle (N − 1)'s taps.
//!
//! All buffers are bounded to `retention = 2*cycle_period + training_len +
//! n_taps` symbols (the fractional input keeps `pitch_fse×` as many
//! samples). Two cycles is the minimum the bootstrap LS estimator needs.

use crate::types::Complex64;

/// Ridge (diagonal-loading) factor for the fractionally-spaced LS solve,
/// relative to the mean Gram diagonal. A T/2 matched-filter window is
/// heavily oversampled, so on a clean / high-SNR signal the normal-
/// equations Gram is near rank-deficient and an unregularised solve picks
/// a huge-norm noise-amplifying equaliser (observed `tap_norm ≈ 54`,
/// preamble reconstruction RMS ≈ 1.2 — total failure). Diagonal loading
/// selects the minimum-norm solution instead; it is negligible against
/// real channel noise but rescues the noiseless / loopback case the batch
/// path never had to handle. This is why the streaming FFE uses its own LS
/// rather than the OTA-validated `ffe::train_ffe_ls`.
const FFE_RIDGE_REL: f64 = 1e-2;

/// Streaming fractionally-spaced FFE block. See module docs.
pub struct StreamingFfe {
    /// Fractional FFE length (taps at T/`pitch_fse` spacing).
    n_taps: usize,
    /// Fractional samples per symbol of the input stream (2 for T/2).
    pitch_fse: usize,
    /// Matched-filter group delay in fractional samples. The on-symbol
    /// grid `frac[i*pitch_fse]` sits on the MF rising edge, this many
    /// samples *before* the symbol's MF peak; the FFE convolution centre
    /// is shifted right by `mf_delay_frac` so the (narrow) tap window sits
    /// on the peak — without this the symbol energy falls outside the
    /// window and the equaliser cannot reconstruct it. `raw_buf` keeps the
    /// phase-0 grid (so acquisition's offset convention is unchanged).
    mf_delay_frac: usize,
    current_taps: Option<Vec<Complex64>>,
    /// Optional forward (between-anchor) decision-directed LMS. When set,
    /// `push_raw` adapts `current_taps` on every emitted symbol using
    /// `forward_slice` as the decision and `forward_mu` as the NLMS rate —
    /// the streaming analogue of the batch path's continuous DD-LMS over
    /// the whole burst (the static-between-anchors forward apply otherwise
    /// lets DATA cycles far from an anchor drift). Off until
    /// [`set_forward_lms`](StreamingFfe::set_forward_lms) is called.
    forward_slice: Option<Box<dyn Fn(Complex64) -> Complex64 + Send + Sync>>,
    forward_mu: f64,
    /// Fractionally-spaced (T/`pitch_fse`) matched-filter samples.
    frac_buf: Vec<Complex64>,
    /// On-symbol decimation of `frac_buf` (= `frac_buf[i*pitch_fse]`),
    /// one entry per emitted symbol. The un-equalised symbol stream.
    raw_sym_buf: Vec<Complex64>,
    /// Equalised symbol stream, one entry per emitted symbol.
    out_buf: Vec<Complex64>,
    /// Absolute symbol index of `out_buf[0]` / `raw_sym_buf[0]`. The
    /// fractional sample for the symbol at `start_abs` sits at
    /// `frac_buf[0]` (on-symbol aligned, invariant preserved by `trim`).
    start_abs: u64,
    /// Retention bound in symbols.
    retention: usize,
}

impl StreamingFfe {
    /// Construct a fresh block. `n_taps` is the fractional FFE length
    /// (T/`pitch_fse`), `cycle_period` the full SOF-to-SOF spacing in
    /// symbols, `training_len` the worst-case number of reference symbols
    /// an FFE-train pass needs behind the SOF, and `pitch_fse` the number
    /// of fractional input samples per symbol (2 = T/2).
    pub fn new(
        n_taps: usize,
        cycle_period: usize,
        training_len: usize,
        pitch_fse: usize,
        mf_delay_frac: usize,
    ) -> Self {
        let retention = 2 * cycle_period + training_len + n_taps;
        Self {
            n_taps,
            pitch_fse: pitch_fse.max(1),
            mf_delay_frac,
            current_taps: None,
            forward_slice: None,
            forward_mu: 0.0,
            frac_buf: Vec::with_capacity(retention * pitch_fse.max(1)),
            raw_sym_buf: Vec::with_capacity(retention),
            out_buf: Vec::with_capacity(retention),
            start_abs: 0,
            retention,
        }
    }

    /// Drop all retained samples and taps. The next `push_raw` is a
    /// pass-through.
    pub fn reset(&mut self, start_abs: u64) {
        self.current_taps = None;
        self.frac_buf.clear();
        self.raw_sym_buf.clear();
        self.out_buf.clear();
        self.start_abs = start_abs;
    }

    /// Absolute symbol index of the first retained symbol.
    pub fn start_abs(&self) -> u64 {
        self.start_abs
    }

    /// Number of symbols currently retained (same length for raw and
    /// equalised buffers).
    pub fn len(&self) -> usize {
        self.out_buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.out_buf.is_empty()
    }

    /// Equalised symbol stream, length `len()`, starting at `start_abs()`.
    pub fn out_buf(&self) -> &[Complex64] {
        &self.out_buf
    }

    /// Raw (un-equalised) on-symbol stream, length `len()`, starting at
    /// `start_abs()`. Used by the FSM's PLHEADER/marker probe.
    pub fn raw_buf(&self) -> &[Complex64] {
        &self.raw_sym_buf
    }

    /// `true` once `train_at` has installed taps at least once.
    pub fn has_taps(&self) -> bool {
        self.current_taps.is_some()
    }

    /// Enable continuous forward DD-LMS in `push_raw`. `slice` returns the
    /// nearest data-constellation point for a soft symbol; `mu` is the NLMS
    /// step (0 disables the update). Passed as a closure to stay decoupled
    /// from any concrete `Constellation` type.
    pub fn set_forward_lms(
        &mut self,
        slice: Box<dyn Fn(Complex64) -> Complex64 + Send + Sync>,
        mu: f64,
    ) {
        self.forward_slice = Some(slice);
        self.forward_mu = mu;
    }

    /// Append `new_frac` (T/`pitch_fse` samples) to the retention window
    /// and equalise the symbols whose peak FIR window is now fully present
    /// (deferred, in-order — each symbol is emitted exactly once, so the
    /// optional forward DD-LMS adaptation stays causal). Without taps the
    /// emit is on-symbol pass-through. Trims oldest symbols once the buffer
    /// grows past `retention`.
    pub fn push_raw(&mut self, new_frac: &[Complex64]) {
        if new_frac.is_empty() {
            return;
        }
        // Forward-apply gate. Default = enabled (cycle (N-1) taps clean up
        // cycle N's PLHEADER). `STREAMING_FFE_NO_FORWARD=1` makes push_raw
        // pass-through — used to diagnose drift-induced LTI violations.
        let no_forward = std::env::var_os("STREAMING_FFE_NO_FORWARD").is_some();
        let pitch = self.pitch_fse;
        let n_ff = self.n_taps;
        let half = n_ff / 2;
        self.frac_buf.extend_from_slice(new_frac);
        let frac_len = self.frac_buf.len();

        let mut i = self.out_buf.len();
        loop {
            let on_sym = i * pitch;
            if on_sym >= frac_len {
                break;
            }
            let center = on_sym + self.mf_delay_frac;
            let have_taps = !no_forward && self.current_taps.is_some();
            let window_ready = center >= half && center + (n_ff - half) <= frac_len;
            if have_taps && !window_ready {
                // Defer: the peak window is not in the buffer yet. (Without
                // taps we emit the raw on-symbol sample immediately.)
                break;
            }
            let y = if have_taps && window_ready {
                let taps = self.current_taps.as_ref().unwrap();
                let lo = center - half;
                let mut acc = Complex64::new(0.0, 0.0);
                for (t, &tap) in taps.iter().enumerate() {
                    acc += tap * self.frac_buf[lo + t];
                }
                // Continuous forward DD-LMS (NLMS) update on the data
                // decision, adapting the taps for the symbols ahead.
                // Error-gated: only adapt when the decision is confident
                // (|e| < |d|). On silence/noise the FFE output is
                // uncorrelated with any constellation point (|e| ≳ |d|), so
                // skipping there stops the taps diverging during inter-burst
                // gaps (which otherwise breaks the EOT re-acquire that reads
                // the equalised stream).
                if self.forward_mu > 0.0 {
                    if let Some(slice) = self.forward_slice.as_ref() {
                        let d = slice(acc);
                        let e = d - acc;
                        if e.norm_sqr() < d.norm_sqr() {
                            let mut r_pow = 1e-12f64;
                            for t in 0..n_ff {
                                r_pow += self.frac_buf[lo + t].norm_sqr();
                            }
                            let mu_eff = self.forward_mu / r_pow;
                            let taps = self.current_taps.as_mut().unwrap();
                            for t in 0..n_ff {
                                taps[t] +=
                                    Complex64::new(mu_eff, 0.0) * e * self.frac_buf[lo + t].conj();
                            }
                        }
                    }
                }
                acc
            } else {
                // Pass-through / boundary fallback: on-symbol sample.
                self.frac_buf[on_sym]
            };
            self.raw_sym_buf.push(self.frac_buf[on_sym]);
            self.out_buf.push(y);
            i += 1;
        }
        debug_assert_eq!(self.raw_sym_buf.len(), self.out_buf.len());
        self.trim();
    }

    /// Locate the integer fractional delay between the on-symbol grid and
    /// the MF peak by minimising the LS reconstruction error of the known
    /// reference symbols over a sweep of candidate centre offsets — the ML
    /// timing estimate the batch path gets for free from its sync-located
    /// fse grid. `bases` are the phase-0 frac indices `rel_sym*pitch_fse`
    /// of the refs. The pipeline group delay (MF + resampler) is a few
    /// symbols and constant, so a generous sweep around the nominal RRC
    /// delay reliably brackets it; the FFE then absorbs the sub-sample
    /// residual.
    fn calibrate_delay(&self, bases: &[usize], sym_refs: &[Complex64]) -> usize {
        let n_ff = self.n_taps;
        let half = n_ff / 2;
        // Sweep from 0 up to ~2× the nominal RRC delay plus a few symbols.
        let search_max = self.mf_delay_frac * 2 + 4 * self.pitch_fse;
        let mut best = (self.mf_delay_frac, f64::INFINITY);
        for d in 0..=search_max {
            // Every centre must have a full FIR window in the buffer.
            if bases
                .iter()
                .any(|&b| b + d + (n_ff - half) > self.frac_buf.len())
            {
                continue;
            }
            let positions: Vec<usize> = bases.iter().map(|&b| b + d).collect();
            let taps = train_ls_ridge(&self.frac_buf, sym_refs, &positions, n_ff);
            let mut err = 0.0f64;
            let mut cnt = 0usize;
            for (p, &center) in positions.iter().enumerate() {
                if center < half {
                    continue;
                }
                let mut y = Complex64::new(0.0, 0.0);
                for (i, &t) in taps.iter().enumerate() {
                    y += t * self.frac_buf[center - half + i];
                }
                err += (y - sym_refs[p]).norm_sqr();
                cnt += 1;
            }
            if cnt > 0 {
                let rms = (err / cnt as f64).sqrt();
                if rms < best.1 {
                    best = (d, rms);
                }
            }
        }
        if std::env::var_os("V3_FFE_DBG").is_some() {
            eprintln!(
                "[ffe_dbg] calibrate: nominal={} best_delay={} best_rms={:.4} frac_len={}",
                self.mf_delay_frac,
                best.0,
                best.1,
                self.frac_buf.len(),
            );
        }
        best.0
    }

    /// LS-train fresh fractional taps from `refs` (absolute symbol index →
    /// expected symbol) and re-equalise `out_buf[sof_abs .. sof_abs +
    /// cycle_period]` (symbols) with the new taps. Refs outside the
    /// retained window are skipped — the LS solver uses whatever survives.
    ///
    /// Returns `false` when fewer than `n_taps` refs survive (the fractional
    /// LS solve would be under-determined).
    pub fn train_at(
        &mut self,
        sof_abs: u64,
        refs: &[(u64, Complex64)],
        cycle_period: usize,
    ) -> bool {
        let pitch = self.pitch_fse;
        let mut positions: Vec<usize> = Vec::with_capacity(refs.len());
        let mut sym_refs: Vec<Complex64> = Vec::with_capacity(refs.len());
        for &(abs, sym) in refs {
            if abs < self.start_abs {
                continue;
            }
            let rel_sym = (abs - self.start_abs) as usize;
            let center = rel_sym * pitch + self.mf_delay_frac;
            if center >= self.frac_buf.len() {
                continue;
            }
            positions.push(center);
            sym_refs.push(sym);
        }
        if positions.len() < self.n_taps {
            return false;
        }
        let taps = train_ls_ridge(&self.frac_buf, &sym_refs, &positions, self.n_taps);
        if sof_abs >= self.start_abs {
            let sof_rel = (sof_abs - self.start_abs) as usize;
            let cycle_end = (sof_rel + cycle_period).min(self.out_buf.len());
            for i in sof_rel..cycle_end {
                self.out_buf[i] =
                    equalise_sym(&self.frac_buf, i, Some(&taps), self.n_taps, pitch, self.mf_delay_frac);
            }
        }
        self.current_taps = Some(taps);
        true
    }

    /// LS-init then **DD-LMS** re-equalise from an anchor — the streaming
    /// equivalent of the batch `rx_v2` path (`train_ffe_ls` →
    /// `apply_ffe_lms_with_training`). The pure LS init alone is too ill-
    /// conditioned at T/2 to decode on a real RRC channel; the DD-LMS pass
    /// refines it using the known `refs` (preamble + LMS warmup,
    /// `mu_train`) and `slice` (the caller's data-constellation nearest-
    /// point decision) on every other symbol (`mu_dd`). The final adapted
    /// taps become `current_taps` and forward-apply to the DATA symbols
    /// streaming in after the anchor.
    ///
    /// `slice` is passed as a closure so the block stays decoupled from any
    /// particular `Constellation` type (the V3 and base crates each carry
    /// their own).
    ///
    /// Returns `false` when fewer than `n_taps` refs survive in-window.
    pub fn train_lms_at(
        &mut self,
        sof_abs: u64,
        refs: &[(u64, Complex64)],
        cycle_period: usize,
        slice: impl Fn(Complex64) -> Complex64,
        mu_train: f64,
        mu_dd: f64,
    ) -> bool {
        let pitch = self.pitch_fse;
        let n_ff = self.n_taps;
        let half = n_ff / 2;
        // In-window reference symbols and their on-symbol (phase-0) frac
        // bases. The actual training centres add the calibrated MF delay.
        let mut bases: Vec<usize> = Vec::with_capacity(refs.len());
        let mut sym_refs: Vec<Complex64> = Vec::with_capacity(refs.len());
        for &(abs, sym) in refs {
            if abs < self.start_abs {
                continue;
            }
            bases.push((abs - self.start_abs) as usize * pitch);
            sym_refs.push(sym);
        }
        if sym_refs.len() < n_ff {
            return false;
        }
        // Calibrate the MF-peak delay from the preamble timing (the ML
        // estimate), then commit it so the forward `push_raw` and later
        // anchors share the same window centring.
        self.mf_delay_frac = self.calibrate_delay(&bases, &sym_refs);
        let mut positions: Vec<usize> = Vec::with_capacity(bases.len());
        let mut fit_refs: Vec<Complex64> = Vec::with_capacity(bases.len());
        for (&b, &r) in bases.iter().zip(sym_refs.iter()) {
            let c = b + self.mf_delay_frac;
            if c < self.frac_buf.len() {
                positions.push(c);
                fit_refs.push(r);
            }
        }
        if positions.len() < n_ff {
            return false;
        }
        let mut taps = train_ls_ridge(&self.frac_buf, &fit_refs, &positions, n_ff);

        if sof_abs < self.start_abs {
            // Anchor already trimmed out of the window — keep the LS taps
            // for forward apply, but there's nothing in-buffer to refine on.
            self.current_taps = Some(taps);
            return true;
        }
        let sof_rel = (sof_abs - self.start_abs) as usize;
        let cycle_end = (sof_rel + cycle_period).min(self.out_buf.len());
        if cycle_end <= sof_rel {
            self.current_taps = Some(taps);
            return true;
        }

        // Training references relative to the SOF symbol, ascending in k
        // (refs arrive preamble-then-warmup, already sorted).
        let training: Vec<(usize, Complex64)> = refs
            .iter()
            .filter_map(|&(abs, sym)| {
                if abs < sof_abs {
                    return None;
                }
                let k = (abs - sof_abs) as usize;
                (k < cycle_end - sof_rel).then_some((k, sym))
            })
            .collect();

        // Forward DD-LMS pass over [sof_rel, cycle_end), mirroring
        // `ffe::apply_ffe_lms_with_training` but with a closure slicer.
        let mut train_cursor = 0usize;
        for k in 0..(cycle_end - sof_rel) {
            let center = (sof_rel + k) * pitch + self.mf_delay_frac;
            if center < half || center + (n_ff - half) > self.frac_buf.len() {
                // Boundary symbol: leave the (forward-applied or raw) value.
                continue;
            }
            let lo = center - half;
            let mut y = Complex64::new(0.0, 0.0);
            for (i, &t) in taps.iter().enumerate() {
                y += t * self.frac_buf[lo + i];
            }
            self.out_buf[sof_rel + k] = y;

            while train_cursor < training.len() && training[train_cursor].0 < k {
                train_cursor += 1;
            }
            let (d, mu) = if train_cursor < training.len() && training[train_cursor].0 == k {
                (training[train_cursor].1, mu_train)
            } else {
                (slice(y), mu_dd)
            };
            if mu > 0.0 {
                let e = d - y;
                let mut r_pow = 1e-12f64;
                for i in 0..n_ff {
                    r_pow += self.frac_buf[lo + i].norm_sqr();
                }
                let mu_eff = mu / r_pow;
                for i in 0..n_ff {
                    taps[i] += Complex64::new(mu_eff, 0.0) * e * self.frac_buf[lo + i].conj();
                }
            }
        }
        self.current_taps = Some(taps);
        true
    }

    fn trim(&mut self) {
        if self.out_buf.len() > self.retention {
            let drop = self.out_buf.len() - self.retention;
            self.out_buf.drain(..drop);
            self.raw_sym_buf.drain(..drop);
            // Drop whole symbols' worth of fractional samples so frac_buf[0]
            // stays on-symbol aligned for `start_abs`.
            let drop_frac = (drop * self.pitch_fse).min(self.frac_buf.len());
            self.frac_buf.drain(..drop_frac);
            self.start_abs += drop as u64;
        }
    }
}

/// Equalise the symbol at local index `i_local` from the fractional input.
/// `center = i_local * pitch_fse` is the on-symbol grid point. Boundary
/// positions where the FIR window would over- or under-flow fall through
/// to the raw on-symbol sample.
fn equalise_sym(
    frac: &[Complex64],
    i_local: usize,
    taps: Option<&[Complex64]>,
    n_taps: usize,
    pitch_fse: usize,
    mf_delay_frac: usize,
) -> Complex64 {
    // On-symbol (phase-0) sample, used for pass-through / boundary fallback.
    let on_sym = i_local * pitch_fse;
    let taps = match taps {
        Some(t) => t,
        None => return frac[on_sym],
    };
    let half = n_taps / 2;
    // Convolution centre is shifted right by the MF group delay so the tap
    // window sits on the symbol's MF peak rather than its rising edge.
    let center = on_sym + mf_delay_frac;
    if center < half || center + (n_taps - half) > frac.len() {
        return frac[on_sym];
    }
    let mut y = Complex64::new(0.0, 0.0);
    for (t, &tap) in taps.iter().enumerate() {
        y += tap * frac[center - half + t];
    }
    y
}

/// LS-train a complex FFE on known reference symbols with **diagonal
/// loading** `FFE_RIDGE_REL`. Same normal-equations setup as
/// `ffe::train_ffe_ls`, but `A^H A + λI` is solved instead of `A^H A`,
/// so a rank-deficient (oversampled / noiseless) Gram yields the minimum-
/// norm equaliser rather than a blown-up one. See `FFE_RIDGE_REL`.
fn train_ls_ridge(
    fse_input: &[Complex64],
    refs: &[Complex64],
    positions: &[usize],
    n_ff: usize,
) -> Vec<Complex64> {
    let half = n_ff / 2;
    let zero = Complex64::new(0.0, 0.0);
    let mut gram = vec![vec![zero; n_ff]; n_ff];
    let mut rhs = vec![zero; n_ff];
    for (k, &center) in positions.iter().enumerate() {
        if center < half {
            continue;
        }
        let lo = center - half;
        let hi = center + half + 1;
        if hi > fse_input.len() {
            continue;
        }
        let r = &fse_input[lo..hi];
        let b = refs[k];
        for i in 0..n_ff {
            rhs[i] += r[i].conj() * b;
            for j in 0..n_ff {
                gram[i][j] += r[i].conj() * r[j];
            }
        }
    }
    // Diagonal loading proportional to the mean Gram diagonal.
    let mut diag_sum = 0.0f64;
    for i in 0..n_ff {
        diag_sum += gram[i][i].re;
    }
    let lambda = FFE_RIDGE_REL * (diag_sum / n_ff as f64).max(1e-12);
    for i in 0..n_ff {
        gram[i][i] += Complex64::new(lambda, 0.0);
    }
    match gauss_solve(gram, rhs) {
        Some(h) => h,
        None => {
            let mut fallback = vec![zero; n_ff];
            fallback[half] = Complex64::new(1.0, 0.0);
            fallback
        }
    }
}

/// Gauss-Jordan elimination with partial pivoting on a complex square
/// system. Returns `Some(x)` solving `a*x = b`, or `None` if singular.
/// (Local copy so the streaming ridge LS does not depend on the private
/// solver inside `ffe`.)
fn gauss_solve(mut a: Vec<Vec<Complex64>>, mut b: Vec<Complex64>) -> Option<Vec<Complex64>> {
    let n = b.len();
    if a.len() != n || a.iter().any(|row| row.len() != n) {
        return None;
    }
    for p in 0..n {
        let mut pivot_row = p;
        let mut pivot_mag = a[p][p].norm();
        for i in (p + 1)..n {
            let m = a[i][p].norm();
            if m > pivot_mag {
                pivot_mag = m;
                pivot_row = i;
            }
        }
        if pivot_mag < 1e-20 {
            return None;
        }
        a.swap(p, pivot_row);
        b.swap(p, pivot_row);
        let pivot = a[p][p];
        for j in p..n {
            a[p][j] = a[p][j] / pivot;
        }
        b[p] = b[p] / pivot;
        let pivot_row_snapshot: Vec<Complex64> = a[p][p..n].to_vec();
        let b_p = b[p];
        for i in 0..n {
            if i == p {
                continue;
            }
            let factor = a[i][p];
            if factor.norm_sqr() < 1e-30 {
                continue;
            }
            for (col_offset, &pv) in pivot_row_snapshot.iter().enumerate() {
                a[i][p + col_offset] -= factor * pv;
            }
            b[i] -= factor * b_p;
        }
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_no_taps_symbol_spaced() {
        // pitch_fse = 1 → degenerate symbol-spaced behaviour.
        let mut ffe = StreamingFfe::new(8, 100, 16, 1, 0);
        let raw: Vec<Complex64> = (0..50).map(|i| Complex64::new(i as f64, 0.0)).collect();
        ffe.push_raw(&raw);
        assert_eq!(ffe.len(), raw.len());
        assert_eq!(ffe.out_buf(), raw.as_slice());
        assert_eq!(ffe.raw_buf(), raw.as_slice());
        assert!(!ffe.has_taps());
    }

    #[test]
    fn passthrough_t2_decimates_on_symbol() {
        // pitch_fse = 2: raw_buf is the on-symbol decimation, out_buf == raw.
        let mut ffe = StreamingFfe::new(9, 100, 16, 2, 0);
        let frac: Vec<Complex64> = (0..50).map(|i| Complex64::new(i as f64, 0.0)).collect();
        ffe.push_raw(&frac);
        assert_eq!(ffe.len(), 25);
        for s in 0..25 {
            // on-symbol sample is frac[2s]
            assert_eq!(ffe.raw_buf()[s], frac[2 * s]);
            assert_eq!(ffe.out_buf()[s], frac[2 * s]);
        }
        assert!(!ffe.has_taps());
    }

    #[test]
    fn train_at_identity_channel_t2() {
        // Clean T/2 channel: on-symbol samples carry the symbol, the
        // half-symbol samples are interpolated neighbours. LS should
        // recover the symbol refs.
        let pitch = 2;
        let n_sym = 60;
        let refs: Vec<Complex64> = (0..n_sym)
            .map(|k| {
                let phase = (k % 4) as f64 * std::f64::consts::PI / 2.0;
                Complex64::new(phase.cos(), phase.sin())
            })
            .collect();
        // Build a T/2 stream: on-symbol = ref, half = midpoint of neighbours.
        let mut frac = vec![Complex64::new(0.0, 0.0); n_sym * pitch];
        for k in 0..n_sym {
            frac[2 * k] = refs[k];
            let nxt = if k + 1 < n_sym { refs[k + 1] } else { refs[k] };
            frac[2 * k + 1] = (refs[k] + nxt) * Complex64::new(0.5, 0.0);
        }
        let mut ffe = StreamingFfe::new(9, 50, 16, pitch, 0);
        ffe.push_raw(&frac);
        let train_refs: Vec<(u64, Complex64)> = (10..40).map(|k| (k as u64, refs[k])).collect();
        let ok = ffe.train_at(10, &train_refs, 30);
        assert!(ok, "train_at failed on clean T/2 channel");
        assert!(ffe.has_taps());
        // Tolerance reflects the diagonal-loading (ridge) bias in
        // `train_ls_ridge` — the LS no longer fits to machine precision.
        for k in 12..38 {
            let err = (ffe.out_buf()[k] - refs[k]).norm();
            assert!(err < 5e-2, "k={k} err={err}");
        }
    }

    #[test]
    fn retention_trims_from_head_t2() {
        let cycle = 100;
        let train = 16;
        let n_taps = 9;
        let pitch = 2;
        let retention = 2 * cycle + train + n_taps;
        let mut ffe = StreamingFfe::new(n_taps, cycle, train, pitch, 0);
        let n_sym = retention + 50;
        let frac: Vec<Complex64> = (0..n_sym * pitch)
            .map(|i| Complex64::new(i as f64, 0.0))
            .collect();
        ffe.push_raw(&frac);
        assert_eq!(ffe.len(), retention);
        assert_eq!(ffe.start_abs(), 50);
        // First retained symbol is symbol 50 → on-symbol frac[2*50].
        assert_eq!(ffe.raw_buf()[0], frac[2 * 50]);
        assert_eq!(ffe.raw_buf()[retention - 1], frac[2 * (n_sym - 1)]);
    }

    #[test]
    fn train_at_silently_skips_when_refs_too_few() {
        let mut ffe = StreamingFfe::new(9, 50, 16, 2, 0);
        let frac: Vec<Complex64> = (0..120).map(|i| Complex64::new(i as f64, 0.0)).collect();
        ffe.push_raw(&frac);
        let refs: Vec<(u64, Complex64)> = (10..14).map(|i| (i as u64, frac[2 * i])).collect();
        assert!(!ffe.train_at(10, &refs, 30));
        assert!(!ffe.has_taps());
    }

    #[test]
    fn reset_returns_to_passthrough() {
        let mut ffe = StreamingFfe::new(9, 50, 16, 2, 0);
        let frac: Vec<Complex64> = (0..120).map(|i| Complex64::new(i as f64, 0.0)).collect();
        ffe.push_raw(&frac);
        let refs: Vec<(u64, Complex64)> = (10..40).map(|i| (i as u64, frac[2 * i])).collect();
        ffe.train_at(10, &refs, 30);
        assert!(ffe.has_taps());
        ffe.reset(1234);
        assert!(!ffe.has_taps());
        assert_eq!(ffe.len(), 0);
        assert_eq!(ffe.start_abs(), 1234);
    }

    #[test]
    fn chunked_push_matches_monolithic_t2() {
        // Byte-exactness across chunk sizes (the [[streaming-ffe-junction-bug]]
        // invariant): push the same T/2 stream whole vs in small chunks and
        // require identical out_buf after a train.
        let pitch = 2;
        let n_sym = 200;
        let refs: Vec<Complex64> = (0..n_sym)
            .map(|k| {
                let p = (k * 7 % 4) as f64 * std::f64::consts::PI / 2.0;
                Complex64::new(p.cos(), p.sin())
            })
            .collect();
        let h = [
            Complex64::new(0.2, 0.05),
            Complex64::new(1.0, 0.0),
            Complex64::new(0.25, -0.1),
        ];
        let mut frac = vec![Complex64::new(0.0, 0.0); n_sym * pitch];
        for k in 0..n_sym {
            for (di, &hi) in h.iter().enumerate() {
                let idx = 2 * k as isize + di as isize - 1;
                if idx >= 0 && (idx as usize) < frac.len() {
                    frac[idx as usize] += hi * refs[k];
                }
            }
        }
        let build = |chunk: usize| {
            let mut ffe = StreamingFfe::new(9, 100, 16, pitch, 0);
            if chunk == 0 {
                ffe.push_raw(&frac);
            } else {
                for c in frac.chunks(chunk) {
                    ffe.push_raw(c);
                }
            }
            let tr: Vec<(u64, Complex64)> = (4..40).map(|k| (k as u64, refs[k])).collect();
            ffe.train_at(4, &tr, 60);
            // Push more so forward-apply runs after train.
            ffe.out_buf().to_vec()
        };
        let mono = build(0);
        for &ch in &[3usize, 5, 7, 16, 33] {
            let c = build(ch);
            assert_eq!(mono.len(), c.len(), "len differs at chunk {ch}");
            for (k, (a, b)) in mono.iter().zip(c.iter()).enumerate() {
                assert!((a - b).norm() < 1e-9, "chunk {ch} sym {k}: {a:?} vs {b:?}");
            }
        }
    }
}
