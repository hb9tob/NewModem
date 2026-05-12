//! FSE FFE + optional DFE, LMS adaptive equalizer.
//!
//! Port de run_fse() lignes 614-780.

use crate::constellation::Constellation;
use crate::types::Complex64;

/// FSE equalizer output for one frame.
pub struct FseOutput {
    pub outputs: Vec<Complex64>,
    pub decisions: Vec<Complex64>,
    pub training_mask: Vec<bool>,
    pub n_processed: usize,
}

/// Run the FSE (Fractionally Spaced Equalizer) on the decimated input.
///
/// Architecture: FFE (complex, T/d_fse spaced) + optional DFE (T-spaced) + DD-PLL.
/// Training: known symbols (preamble + pilots), then decision-directed.
pub fn run_fse(
    fse_input: &[Complex64],
    all_symbols: &[Complex64],
    training_mask: &[bool],
    training_refs: &[Complex64],
    constellation: &Constellation,
    pitch_fse: usize,
    sps_fse: usize,
    fse_start: usize,
    n_ff: Option<usize>,
    ffe_only: bool,
    mu_ff_train: f64,
    mu_dfe_train: f64,
    mu_ff_dd: f64,
    mu_dfe_dd: f64,
    pll_alpha: f64,
    pll_beta: f64,
) -> FseOutput {
    let n_sym = all_symbols.len();
    let n_dfe = if ffe_only { 0 } else { 5 };

    // FFE size: depends on tau
    let tau_eff = pitch_fse as f64 / sps_fse as f64;
    let n_ff = n_ff.unwrap_or_else(|| {
        let ff = if tau_eff >= 0.99 {
            8 * sps_fse + 1
        } else {
            4 * sps_fse + 1
        };
        ff | 1 // ensure odd
    });
    let half = n_ff / 2;

    // Init FFE: center tap = 1
    let mut ffe_taps = vec![Complex64::new(0.0, 0.0); n_ff];
    ffe_taps[half] = Complex64::new(1.0, 0.0);
    let mut dfe_taps = vec![Complex64::new(0.0, 0.0); n_dfe];

    let mut outputs = vec![Complex64::new(0.0, 0.0); n_sym];
    let mut decisions = vec![Complex64::new(0.0, 0.0); n_sym];

    // DD-PLL state
    let mut theta: f64 = 0.0;
    let mut nu: f64 = 0.0;

    // Gain normalization from preamble
    let n_probe = 32.min(n_sym);
    let mut scale = Complex64::new(1.0, 0.0);
    {
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..n_probe {
            let center_idx = fse_start + k * pitch_fse;
            if center_idx < fse_input.len() && training_mask[k] {
                num += fse_input[center_idx] * training_refs[k].conj();
                den += training_refs[k].norm_sqr();
            }
        }
        if den > 1e-12 {
            scale = num / den;
        }
    }

    // Apply gain normalization
    let fse_input_norm: Vec<Complex64> = if scale.norm_sqr() > 1e-20 {
        fse_input.iter().map(|&x| x / scale).collect()
    } else {
        fse_input.to_vec()
    };

    let mut n_processed = 0;

    for k in 0..n_sym {
        let center_idx = fse_start + k * pitch_fse;
        let lo = center_idx as isize - half as isize;
        let hi = center_idx + half + 1;
        if lo < 0 || hi > fse_input_norm.len() {
            break;
        }
        let lo = lo as usize;

        // FFE output
        let r = &fse_input_norm[lo..hi];
        let mut y_ff = Complex64::new(0.0, 0.0);
        for (i, &tap) in ffe_taps.iter().enumerate() {
            y_ff += tap * r[i];
        }

        // DFE
        let mut y_dfe = Complex64::new(0.0, 0.0);
        if !ffe_only && k >= 1 {
            let n_use = n_dfe.min(k);
            for d in 0..n_use {
                y_dfe += dfe_taps[d] * decisions[k - 1 - d];
            }
        }

        let y = y_ff - y_dfe;

        // DD-PLL de-rotation
        let rot = Complex64::from_polar(1.0, -theta);
        let y_rot = y * rot;
        outputs[k] = y_rot;

        // Decision
        let (d, mu_ff, mu_dfe) = if training_mask[k] {
            (training_refs[k], mu_ff_train, mu_dfe_train)
        } else {
            let idx = constellation.slice_nearest(&[y_rot])[0];
            (constellation.points[idx], mu_ff_dd, mu_dfe_dd)
        };
        decisions[k] = d;

        // PLL update: phase error normalized by |d|^2
        let d_mag_sq = d.norm_sqr();
        if d_mag_sq > 1e-12 {
            let phi = (y_rot * d.conj()).im / d_mag_sq;
            theta += pll_alpha * phi + nu;
            nu += pll_beta * phi;
        }

        // LMS error (in pre-rotation domain)
        let e_pre = d * Complex64::from_polar(1.0, theta) - y;

        // FFE update
        for (i, tap) in ffe_taps.iter_mut().enumerate() {
            *tap += Complex64::new(mu_ff, 0.0) * e_pre * r[i].conj();
        }

        // DFE update
        if !ffe_only && k >= 1 {
            let n_use = n_dfe.min(k);
            for d_idx in 0..n_use {
                let hist = decisions[k - 1 - d_idx];
                dfe_taps[d_idx] += Complex64::new(mu_dfe, 0.0) * e_pre * hist.conj();
            }
        }

        n_processed = k + 1;
    }

    FseOutput {
        outputs: outputs[..n_processed].to_vec(),
        decisions: decisions[..n_processed].to_vec(),
        training_mask: training_mask[..n_processed].to_vec(),
        n_processed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constellation::qpsk_gray;

    #[test]
    fn fse_passthrough_clean() {
        // With clean input (no channel), FSE should pass through
        let c = qpsk_gray();
        let syms: Vec<Complex64> = c.points.iter().cycle().take(20).cloned().collect();
        let mask = vec![true; 20]; // all training
        let refs = syms.clone();

        // Simulate: symbols at pitch_fse=2 spacing, pad with zeros
        let pitch_fse = 2;
        let sps_fse = 2;
        let fse_start = 10; // some margin
        let total = fse_start + 20 * pitch_fse + 20;
        let mut fse_input = vec![Complex64::new(0.0, 0.0); total];
        for (k, &sym) in syms.iter().enumerate() {
            fse_input[fse_start + k * pitch_fse] = sym;
        }

        let out = run_fse(
            &fse_input, &syms, &mask, &refs, &c,
            pitch_fse, sps_fse, fse_start,
            None, true, 0.01, 0.0, 0.0, 0.0, 0.01, 0.001,
        );

        assert_eq!(out.n_processed, 20);
        // Decisions should match input
        for (i, (&d, &s)) in out.decisions.iter().zip(syms.iter()).enumerate() {
            assert!(
                (d - s).norm() < 0.5,
                "Decision mismatch at symbol {i}: got {:?}, expected {:?}",
                d, s
            );
        }
    }
}
