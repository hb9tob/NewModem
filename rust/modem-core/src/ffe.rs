//! Pilot-trained Feed-Forward Equalizer (FFE) at fractional spacing T/d_fse.
//!
//! Closed-form LS training on a set of known reference symbols (preamble, pilots).
//! The trained FFE is then applied to extract one complex sample per symbol slot,
//! absorbing sub-sample timing offset and mild channel ISI.
//!
//! Usage:
//! 1. Decimate MF with `sync::decimate_for_fse` → fse_input, fse_start, d_fse
//! 2. Call `train_ffe_ls` with preamble (or preamble + pilots) as training references
//! 3. Call `apply_ffe` to extract a symbol-spaced complex stream

use crate::types::Complex64;

/// LS-train a complex FFE on known reference symbols.
///
/// Solves the overdetermined system `A h = b` in the least-squares sense via the
/// normal equations `A^H A h = A^H b`, where each row of A is an `n_ff`-sample
/// window of `fse_input` centered on a training position, and `b[k]` is the
/// known TX symbol at that position.
///
/// Parameters
/// - `fse_input`: decimated matched-filter signal at T/d_fse spacing
/// - `training_refs`: known TX symbols at each training position
/// - `training_positions`: index in `fse_input` of each training symbol's center
/// - `n_ff`: FFE length (odd, centered tap index = n_ff/2)
///
/// Returns the trained complex FFE taps. If the Gram matrix is singular, returns
/// a unit-impulse-at-center (identity FFE).
pub fn train_ffe_ls(
    fse_input: &[Complex64],
    training_refs: &[Complex64],
    training_positions: &[usize],
    n_ff: usize,
) -> Vec<Complex64> {
    assert_eq!(training_refs.len(), training_positions.len());
    let half = n_ff / 2;
    let zero = Complex64::new(0.0, 0.0);

    let mut gram = vec![vec![zero; n_ff]; n_ff];
    let mut rhs = vec![zero; n_ff];

    for (k, &center) in training_positions.iter().enumerate() {
        if center < half {
            continue;
        }
        let lo = center - half;
        let hi = center + half + 1;
        if hi > fse_input.len() {
            break;
        }
        let r = &fse_input[lo..hi];
        let b = training_refs[k];
        for i in 0..n_ff {
            rhs[i] += r[i].conj() * b;
            for j in 0..n_ff {
                gram[i][j] += r[i].conj() * r[j];
            }
        }
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

/// Apply a trained FFE at a sequence of symbol positions.
///
/// For each symbol k=0..n_total_sym, computes
///   y[k] = sum_{i=0..n_ff-1} taps[i] * fse_input[center(k) - half + i]
/// where center(k) = first_center + k * pitch_fse.
///
/// Returns one Complex64 per symbol slot. Positions that would require samples
/// outside `fse_input` bounds return zero.
pub fn apply_ffe(
    fse_input: &[Complex64],
    ffe_taps: &[Complex64],
    first_center: usize,
    pitch_fse: usize,
    n_total_sym: usize,
) -> Vec<Complex64> {
    let n_ff = ffe_taps.len();
    let half = n_ff / 2;
    let zero = Complex64::new(0.0, 0.0);
    let mut out = Vec::with_capacity(n_total_sym);
    for k in 0..n_total_sym {
        let center = first_center + k * pitch_fse;
        if center < half || center + half + 1 > fse_input.len() {
            out.push(zero);
            continue;
        }
        let mut y = zero;
        for i in 0..n_ff {
            y += ffe_taps[i] * fse_input[center - half + i];
        }
        out.push(y);
    }
    out
}

/// Gauss-Jordan elimination with partial pivoting on a complex square system.
/// Returns Some(x) solving a*x = b, or None if a is singular.
fn gauss_solve(mut a: Vec<Vec<Complex64>>, mut b: Vec<Complex64>) -> Option<Vec<Complex64>> {
    let n = b.len();
    if a.len() != n || a.iter().any(|row| row.len() != n) {
        return None;
    }
    for p in 0..n {
        // Partial pivot: row with largest |a[r][p]| for r >= p
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

        // Snapshot pivot row to resolve mutable-vs-immutable borrow
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

    /// On an ideal channel (impulse), FFE should recover the delta-like response
    /// that perfectly reconstructs the training symbols.
    #[test]
    fn ffe_learns_identity_on_ideal_channel() {
        // Simulate: fse_input[fse_start + k*pitch_fse] = training_refs[k]
        // all other samples = 0 (clean impulse channel at symbol rate)
        let pitch_fse = 2;
        let fse_start = 10;
        let n_train = 32;

        let refs: Vec<Complex64> = (0..n_train)
            .map(|k| {
                let phase = (k % 4) as f64 * std::f64::consts::PI / 2.0;
                Complex64::new(phase.cos(), phase.sin())
            })
            .collect();

        let total_len = fse_start + n_train * pitch_fse + 10;
        let mut fse_input = vec![Complex64::new(0.0, 0.0); total_len];
        for (k, &r) in refs.iter().enumerate() {
            fse_input[fse_start + k * pitch_fse] = r;
        }

        let positions: Vec<usize> = (0..n_train).map(|k| fse_start + k * pitch_fse).collect();

        let n_ff = 5;
        let taps = train_ffe_ls(&fse_input, &refs, &positions, n_ff);

        // Apply and compare
        let out = apply_ffe(&fse_input, &taps, fse_start, pitch_fse, n_train);
        for (k, (&y, &r)) in out.iter().zip(refs.iter()).enumerate() {
            assert!(
                (y - r).norm() < 1e-6,
                "symbol {k}: got {:?}, want {:?}",
                y,
                r
            );
        }
    }

    /// FFE inverts a simple linear channel (RC-shaped symbol response with inter-symbol
    /// overlap). Each training symbol contributes energy to neighbouring fse samples,
    /// so the Gram matrix is non-singular and LS converges to a proper equalizer.
    #[test]
    fn ffe_inverts_short_channel() {
        let pitch_fse = 2;
        let fse_start = 10;
        let n_train = 64;

        let refs: Vec<Complex64> = (0..n_train)
            .map(|k| {
                let phase = (k % 4) as f64 * std::f64::consts::PI / 2.0;
                Complex64::new(phase.cos(), phase.sin())
            })
            .collect();

        // Channel impulse response over T/2 spacing: 3-tap, mild ISI
        let h = [
            Complex64::new(0.25, 0.05),
            Complex64::new(1.0, 0.0),
            Complex64::new(0.3, -0.1),
        ];

        let total_len = fse_start + n_train * pitch_fse + 10;
        let mut fse_input = vec![Complex64::new(0.0, 0.0); total_len];
        for (k, &r) in refs.iter().enumerate() {
            let center = fse_start + k * pitch_fse;
            for (di, &hi) in h.iter().enumerate() {
                let idx = center as isize + di as isize - 1;
                if idx >= 0 && (idx as usize) < total_len {
                    fse_input[idx as usize] += hi * r;
                }
            }
        }

        let positions: Vec<usize> = (0..n_train).map(|k| fse_start + k * pitch_fse).collect();
        let n_ff = 9;
        let taps = train_ffe_ls(&fse_input, &refs, &positions, n_ff);
        let out = apply_ffe(&fse_input, &taps, fse_start, pitch_fse, n_train);

        // After LS, FFE should recover training refs (edges excluded due to truncation)
        let mut err_sum = 0.0f64;
        let mut count = 0;
        let margin = 2;
        for k in margin..(n_train - margin) {
            err_sum += (out[k] - refs[k]).norm_sqr();
            count += 1;
        }
        let rms = (err_sum / count as f64).sqrt();
        assert!(rms < 0.05, "FFE failed to invert channel (rms={rms})");
    }
}
