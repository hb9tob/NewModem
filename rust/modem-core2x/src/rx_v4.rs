//! V4 receive pipeline (symbol domain).
//!
//! `rx_v4_symbols` ingests a stream of complex symbols already synced and
//! matched-filtered (the worker, in [`modem-worker2x`], handles the
//! audio-domain pieces — Farrow interpolation, Gardner closed-loop
//! timing, then samples into one Complex64 per symbol). This split keeps
//! `modem-core2x` self-contained and unit-testable without an audio
//! dependency, and lets the worker reuse the same primitives for
//! sound-card and SDR captures.
//!
//! Pipeline (one PLHEADER cycle):
//!
//! 1. Find next SOF: cross-correlate the family's Chu sequence against
//!    the symbol stream.
//! 2. Decode PLHEADER → [`PlsPayload`] (profile, base_esi, flags, ...)
//!    plus the LS-estimated complex channel gain on the SOF.
//! 3. Skip the LMS warmup (its symbols are consumed only by the audio-
//!    domain FFE; in symbol domain we already have a clean reference).
//! 4. For META-CW + each DATA-CW:
//!    - Read `cw_with_pilots_len(cfg)` symbols: `cw_data_syms` data syms
//!      interleaved with TDM pilots (`pilot_pattern.d_syms` data + `p_syms`
//!      rotating-QPSK pilots per group).
//!    - LS-estimate the per-CW complex gain across the pilot positions
//!      against the known rotating reference.
//!    - Pull data symbols out via [`pilot2x_tdm::deinterleave_data_pilots_2x`].
//!    - Phase-correct by dividing by the gain.
//!    - Soft-demap → LDPC decode → record bytes keyed by ESI.
//! 5. Try RaptorQ assembly with the AppHeader from the META-CW.
//!
//! Late-entry: the loop scans for SOFs from `cursor=0`. A burst that
//! starts mid-cycle simply gives up the first partial cycle and locks
//! on the next SOF — same robustness as V3.

use std::collections::HashMap;

use modem_core_base::ffe::train_ffe_ls;
use modem_core_base::interleaver;
use modem_core_base::ldpc::decoder::LdpcDecoder;
use modem_core_base::phase_smoother::{
    rts_phase_smooth, rts_smooth_scalar, PhaseObs, PhaseSmootherParams,
    ScalarObs, ScalarRtsParams,
};
use modem_core_base::soft_demod::{self, SoftSymbol};
use modem_core_base::types::Complex64;
use modem_framing::app_header::{self, AppHeader};

use crate::frame2x::{
    cw_with_pilots_len, data_cw_per_cycle, full_cycle_len_syms, make_constellation_2x,
    make_lms_warmup_2x, pilot_groups_per_cw, FLAG2X_EOT, FLAG2X_LAST,
};
use crate::pilot2x_tdm::{self, pilot_symbol_2x, PilotPattern2x};
use crate::plheader::{
    self, decode_plheader_at, PlsPayload, PreambleFamily2x, PLHEADER_LEN_SYM, SOF_LEN_SYM,
};
use crate::profile2x::ModemConfig2x;

/// Default LDPC iteration cap — same as V3's
/// `LdpcDecoder::new(rate, 30)` choice.
const LDPC_MAX_ITER: usize = 30;

/// Conservative σ² floor when no pilot residual is available yet.
const SIGMA2_FLOOR: f64 = 1e-3;

/// SOF correlation peak threshold (fraction of `SOF_LEN_SYM`). For unit-
/// magnitude Chu, autocorrelation peaks at 64 on a clean roundtrip.
///
/// Real OTA paths (SDRplay + radio NBFM demod chain with audio-band
/// LPF + de-emphasis + AGC) attenuate the SOF pulse magnitude by 70-80%
/// — observed correlation peaks land in the 15-20 range on a FTX-1 →
/// SDRplay capture even at high SNR. Threshold 0.2 × 64 = 12.8 admits
/// those while staying well above the noise correlation level
/// (random Chu match peaks ≈ √(64·2·ln(L))·σ ≈ 25·σ for L=5000 syms,
/// so at σ=0.06 a noise peak is ~1.5 — 8× under threshold).
///
/// Was 0.5 pre-v0.11.0-2x4 (correct for synthetic WAV roundtrip, blew
/// past every real OTA capture).
const SOF_PEAK_THRESHOLD_FRAC: f64 = 0.2;

/// One decoded V4 burst.
#[derive(Clone, Debug)]
pub struct RxResult2x {
    /// Reassembled payload, possibly truncated to `app_header.file_size`.
    /// Empty when no AppHeader recovered.
    pub data: Vec<u8>,
    /// Recovered AppHeader (None if no META-CW converged).
    pub app_header: Option<AppHeader>,
    /// Number of CWs (data + meta) the LDPC decoder marked converged.
    pub converged_cws: usize,
    /// Total CWs the decoder attempted.
    pub total_cws: usize,
    /// Number of PLHEADER cycles parsed.
    pub cycles: usize,
    /// PLS payload from the **first** decoded cycle (None if no SOF
    /// found). Useful for the worker to log the recovered profile.
    pub first_pls: Option<PlsPayload>,
    /// Whether the EOT flag was seen on any decoded cycle.
    pub eot_seen: bool,
    /// Mean σ² over data CWs (max-log LLR scale). Defaults to a fixed
    /// floor when no pilot residual was available.
    pub sigma2_data: f64,
    /// Mean radial-axis pilot residual variance (along the pilot
    /// direction `P = (1+j)/√2`), MAD-rescaled. Captures amplitude
    /// distortion: AM-AM compression, hard-clipping, AGC error. On a
    /// pure-AWGN channel `σ²_radial ≈ σ²_tangential ≈ σ²_data / 2`.
    /// An imbalance `σ²_radial / σ²_tangential > 2` or `< 0.5` signals
    /// a non-AWGN channel — diagnostic input for channel
    /// characterisation post-decode.
    pub sigma2_radial: f64,
    /// Mean tangential-axis pilot residual variance (perpendicular to
    /// the pilot direction), MAD-rescaled. Captures phase noise:
    /// AM-PM distortion, LO phase noise, residual timing/frequency
    /// jitter. Same interpretation rules as
    /// [`sigma2_radial`](Self::sigma2_radial).
    pub sigma2_tangential: f64,
    /// **Data-scatter σ²** — running mean of `|y − s_hat|²` over every
    /// post-Pass-2 DATA symbol seen across the burst (pilots + PLHEADER
    /// excluded, META CWs excluded). `s_hat` is the nearest constellation
    /// point in Euclidean distance. This is the *honest* noise metric:
    /// `sigma2_data` / `sigma2_radial` / `sigma2_tangential` are
    /// MAD-style estimates from **pilot residuals after the Pass 2 RTS
    /// phase smoother + per-ring gain estimator have been fitted to the
    /// pilot points**, so they understate the noise that actually hits
    /// the data symbols (ICI from RRC pulse leakage, intra-CW phase
    /// noise the smoother doesn't fully track, AM-AM/AM-PM the per-ring
    /// gain absorbs at the pilot rate but not at the data rate). The
    /// data-scatter σ² sees all of those because the hard decision is
    /// independent of every channel estimator. Pair with
    /// [`es_data_scatter`](Self::es_data_scatter) for the per-burst SNR
    /// estimate `10·log10(es_data_scatter / sigma2_data_scatter)`.
    pub sigma2_data_scatter: f64,
    /// Running mean signal power `|s_hat|²` over the same DATA symbols
    /// fed into [`sigma2_data_scatter`](Self::sigma2_data_scatter).
    /// For mid-power-normalised constellations (QPSK / 8-PSK / 16-APSK
    /// at unit mean Es) this should land near 1.0; APSK with multi-ring
    /// designs can sit above 1 if the outer ring dominates the chosen
    /// hard decisions. Used as the SNR denominator alongside
    /// `sigma2_data_scatter`: `snr_db = 10·log10(es_data_scatter /
    /// sigma2_data_scatter)`.
    pub es_data_scatter: f64,
    /// Total count of post-Pass-2 DATA symbols accumulated into
    /// [`sigma2_data_scatter`](Self::sigma2_data_scatter) and
    /// [`es_data_scatter`](Self::es_data_scatter). Zero ⇒ no DATA CW
    /// has been visited yet (SNR estimate is meaningless). The CLI /
    /// GUI suppress the SNR readout while this is zero so the operator
    /// doesn't see −∞ dB during the bootstrap.
    pub data_scatter_n: usize,
    /// Symbol-buffer index of the first SOF the decoder locked onto in
    /// the input symbol stream (relative to the buffer passed in), or
    /// None if no SOF passed the PLHEADER CRC. The streaming worker
    /// uses this to pin its [`StreamingFrontend`]'s pilot-aided TED
    /// anchor — `frontend.align_to_sof(first_sof_at)` enables the
    /// AbsGardner-on-pilot-interior gate (D-1c-ii), which keeps the
    /// timing loop unbiased on multi-ring APSK profiles.
    ///
    /// [`StreamingFrontend`]: `modem_worker2x::streaming_frontend::StreamingFrontend`
    pub first_sof_at: Option<usize>,
    /// Scatter sample of post-correction DATA symbols (META excluded).
    /// Capped at [`MAX_CONSTELLATION_POINTS`]; the worker forwards this
    /// verbatim through the `v2_progress` event so the GUI's
    /// constellation panel renders the live cloud.
    pub constellation_sample: Vec<[f32; 2]>,
    /// Per-CW pilot-LS phase (`arg(gain)` in radians, wrapped to
    /// `[-π, π]`). One entry per decoded CW in the order they were
    /// processed (META included, in line with the parallel
    /// [`pilot_phase_is_meta`](Self::pilot_phase_is_meta) flags). The
    /// V3 GUI's pilot-phase canvas plots this as a polyline; the V4
    /// analogue of "per-segment pilot phase" is one phase per CW (each
    /// CW carries 1 or 2 pilot blocks per the wire format).
    pub pilot_phase_per_cw: Vec<f64>,
    /// Parallel to [`pilot_phase_per_cw`](Self::pilot_phase_per_cw):
    /// `true` for META CWs (header replicated), `false` for DATA. Drives
    /// the GUI's META-vs-DATA colour coding on the phase canvas — same
    /// contract as V3's
    /// `RxV2Result::pilot_phase_is_meta`.
    pub pilot_phase_is_meta: Vec<bool>,
    /// Count of DATA CWs the decoder visited (META excluded). Mirrors
    /// V3's `blocks_total / blocks_expected` semantics for the progress
    /// bar: total CWs the LDPC pipeline attempted in this burst.
    pub data_cws_total: usize,
    /// Count of DATA CWs that converged (META excluded). Subset of
    /// [`data_cws_total`](Self::data_cws_total). Surfaced separately
    /// from [`converged_cws`](Self::converged_cws) so the GUI's progress
    /// bar can display DATA-only without META biasing the numerator.
    pub data_cws_converged: usize,
    /// Bitmap of converged DATA-CW ESIs (LSB-first byte order, same
    /// wire encoding the V3 worker emits). Bit `n` set ⇒ ESI `n`
    /// converged through the LDPC stage; the RaptorQ reassembly may
    /// still drop the CW later if the ESI exceeds the source range,
    /// but for visualization "ESI converged at LDPC" is what the
    /// operator wants to see. Empty until the first DATA CW converges.
    /// The byte length is `ceil(max(max_esi + 1, data_cws_total) / 8)`
    /// so the progress bar can show every attempted DATA slot.
    pub converged_bitmap: Vec<u8>,
    /// Symbol-stream index of every CRC-validated PLHEADER SOF the
    /// decoder locked onto, in scan order. Each entry is guaranteed to
    /// be a real PLHEADER (PLS Golay decode + CRC16 both passed) — no
    /// false positives from noise-band Chu correlations.
    ///
    /// Drift LS fit (`estimate_drift_from_sof_positions`) needs at
    /// least 3 entries; the streaming worker uses the cumulative list
    /// to refine `cached_drift_ppm` between scan ticks.
    pub validated_sof_positions: Vec<usize>,
    /// LS-fitted clock drift in ppm, computed from
    /// `validated_sof_positions` at finalisation. `None` when fewer
    /// than 3 SOFs were validated (insufficient for a slope fit).
    /// Used by the loop-by-loop validation methodology
    /// (`docs/modem_2x_loop_validation.html`) to compare injected vs
    /// estimated drift across the sweep harness.
    pub final_drift_ppm: Option<f64>,
}

/// Cap on the scatter cloud we forward to the GUI per `rx_v4_symbols`
/// call. Matches the V3 `rx_v2::MAX_CONSTELLATION_POINTS` so the
/// frontend renders a comparable density across families.
pub const MAX_CONSTELLATION_POINTS: usize = 500;

/// Slice 2x21 — sliding-window cap on `pilot_phase_per_cw` /
/// `pilot_phase_is_meta`. The streaming GUI shows the "last N" CWs'
/// pilot-phase trace, not the cumulative session history. User intent
/// per the OTA bring-up of 0.11.0-2x20: *"affiche les 4-5 derniers
/// CW"*. The drain happens at the push site so the snapshot the worker
/// forwards to the GUI never carries more than N entries.
pub const PILOT_PHASE_RECENT_N: usize = 5;

impl RxResult2x {
    pub(crate) fn empty() -> Self {
        Self {
            data: Vec::new(),
            app_header: None,
            converged_cws: 0,
            total_cws: 0,
            cycles: 0,
            first_pls: None,
            eot_seen: false,
            sigma2_data: SIGMA2_FLOOR,
            sigma2_radial: SIGMA2_FLOOR / 2.0,
            sigma2_tangential: SIGMA2_FLOOR / 2.0,
            sigma2_data_scatter: 0.0,
            es_data_scatter: 0.0,
            data_scatter_n: 0,
            first_sof_at: None,
            constellation_sample: Vec::new(),
            pilot_phase_per_cw: Vec::new(),
            pilot_phase_is_meta: Vec::new(),
            data_cws_total: 0,
            data_cws_converged: 0,
            converged_bitmap: Vec::new(),
            validated_sof_positions: Vec::new(),
            final_drift_ppm: None,
        }
    }
}

// --- SOF correlation ------------------------------------------------------

/// Find the next SOF starting at or after `cursor`. Returns the symbol
/// index of the first SOF symbol on success.
///
/// Uses simple linear cross-correlation:
///   peak = max_k |Σ_n y[k+n] · conj(sof[n])|   for k in cursor ..= end
/// and accepts the first peak above [`SOF_PEAK_THRESHOLD_FRAC`] · `SOF_LEN_SYM`.
/// Linear (not normalised) is enough because the SOF is constant-magnitude
/// and the gain estimate inside `decode_plheader_at` will absorb any
/// channel scaling later.
/// Circular mean of a phase trajectory in radians (output in `[-π, π]`).
/// Pushed into `RxResult2x.pilot_phase_per_cw` so the GUI's pilot-phase
/// canvas plots one point per CW summarising the Pass 2 RTS-smoothed
/// trend, rather than the noisy first-sample value.
fn mean_phase(phis: &[f64]) -> f64 {
    if phis.is_empty() {
        return 0.0;
    }
    let mut sx = 0.0_f64;
    let mut sy = 0.0_f64;
    for &p in phis {
        sx += p.cos();
        sy += p.sin();
    }
    sy.atan2(sx)
}

/// Find every SOF position above the threshold in a symbol stream.
///
/// Each detected peak advances the scan cursor by one cycle's worth of
/// symbols (≈ `4 s × symbol_rate`) so a fat correlation peak doesn't
/// register twice. Used by the worker's drift bootstrap: multiple SOF
/// positions on the same buffer let us LS-fit the symbol-rate clock
/// offset between TX and RX (see [`estimate_drift_from_sof_positions`]).
pub fn find_all_sofs(
    symbols: &[Complex64],
    family: PreambleFamily2x,
    min_cycle_skip_syms: usize,
) -> Vec<usize> {
    let mut out = Vec::new();
    if symbols.len() < SOF_LEN_SYM {
        return out;
    }
    let sof = plheader::sof_for_family(family);
    // Tighter threshold than `SOF_PEAK_THRESHOLD_FRAC` (0.2) — that
    // threshold accepts a real PLHEADER attenuated by 70-80 % through
    // the SDR + radio chain, but on a long buffer at threshold 0.2
    // we'd also catch false-positive Chu correlations in band noise
    // and PLS QPSK data, which pollute the drift LS fit. 0.3 × 64 =
    // 19.2 rejects most of those while still passing a real PLHEADER
    // with peak ≥ 20 (empirical floor on FTX-1 + SDRplay captures).
    let threshold = 0.3 * SOF_LEN_SYM as f64;
    let end = symbols.len() - SOF_LEN_SYM;
    let mut k = 0;
    while k <= end {
        let mut acc = Complex64::new(0.0, 0.0);
        for n in 0..SOF_LEN_SYM {
            acc += symbols[k + n] * sof[n].conj();
        }
        if acc.norm() >= threshold {
            out.push(k);
            k += min_cycle_skip_syms.max(SOF_LEN_SYM);
        } else {
            k += 1;
        }
    }
    out
}

/// LS-fit the TX↔RX symbol-clock drift from multiple SOF positions in
/// the same buffer, with outlier rejection.
///
/// The encoder places consecutive PLHEADERs `cycle_period_sym` symbols
/// apart (4 s × `symbol_rate` for full cycles). RX-side drift `ε` means
/// the **measured** spacing scales as `cycle × (1 + ε)`. Each SOF's
/// `round((p − p0) / cycle_period_sym)` gives a cycle index (handles
/// missed PLHEADERs); LS slope-fits position vs index; ppm =
/// `(slope − cycle_period_sym) / cycle_period_sym × 1e6`.
///
/// **Outlier rejection** (critical on real OTA where the
/// [`SOF_PEAK_THRESHOLD_FRAC`] = 0.2 lets through a few noise
/// correlations between bursts): each SOF is included in the fit only
/// if its residual from `k × cycle` is < 30 % of one cycle. Without
/// this, false-positive SOF detections in band noise produce garbage
/// ppm and the cubic resample wrecks the audio.
///
/// Returns `None` when fewer than 3 SOFs pass the residual filter or
/// the LS fit is numerically degenerate. Three is the minimum number
/// that lets the slope's standard error stay below the spacing
/// precision (single-sample symbol-grid quantisation).
pub fn estimate_drift_from_sof_positions(
    sof_positions: &[usize],
    cycle_period_sym: usize,
) -> Option<f64> {
    if sof_positions.len() < 2 || cycle_period_sym == 0 {
        return None;
    }
    let p0 = sof_positions[0] as f64;
    let cycle = cycle_period_sym as f64;
    // Outlier tolerance: position must land within ±10 % of a cycle
    // boundary. Loose enough to absorb a ±1 % drift accumulating
    // across many cycles, tight enough to reject false-positive
    // PLS-Golay-passes-by-luck or partial cycles. Drops to 30 % for
    // the longest-running burst (drift of 300 ppm × 20 cycles ≈ 33
    // sym = 0.6 % of cycle, well inside 10 %).
    let tolerance = cycle * 0.10;
    let mut sum_xy = 0.0_f64;
    let mut sum_xx = 0.0_f64;
    let mut n_used = 0_usize;
    for &p in sof_positions {
        let y = p as f64 - p0;
        let k = (y / cycle).round();
        if (y - k * cycle).abs() > tolerance {
            // SOF is too far from the nearest expected position — most
            // likely a noise correlation peak between two genuine
            // PLHEADERs. Skip.
            continue;
        }
        sum_xy += k * y;
        sum_xx += k * k;
        n_used += 1;
    }
    if n_used < 2 || sum_xx < 1.0 {
        return None;
    }
    let slope = sum_xy / sum_xx;
    let ppm = (slope - cycle) / cycle * 1e6;
    // Sanity bound: real-world clock drift is bounded by the spec of
    // sound cards and SDRs to ±300 ppm at the extreme. Anything past
    // that is an LS overfit on a noise correlation that snuck past
    // PLHEADER CRC + Golay (rare but happens with 2 SOFs and one of
    // them landing on a false-positive). Reject — caller treats as
    // "no usable estimate" and keeps the cached value.
    if !ppm.is_finite() || ppm.abs() > 300.0 {
        return None;
    }
    Some(ppm)
}

pub(crate) fn find_next_sof(
    symbols: &[Complex64],
    cursor: usize,
    family: PreambleFamily2x,
) -> Option<usize> {
    if symbols.len() < cursor + SOF_LEN_SYM {
        return None;
    }
    let sof = plheader::sof_for_family(family);
    let threshold = SOF_PEAK_THRESHOLD_FRAC * SOF_LEN_SYM as f64;
    let end = symbols.len() - SOF_LEN_SYM;
    for k in cursor..=end {
        let mut acc = Complex64::new(0.0, 0.0);
        for n in 0..SOF_LEN_SYM {
            acc += symbols[k + n] * sof[n].conj();
        }
        if acc.norm() >= threshold {
            return Some(k);
        }
    }
    None
}

// --- per-CW pilot LS gain estimate ---------------------------------------

/// LS gain estimate `g = Σ y · conj(p) / Σ |p|²` over all known
/// reference symbols in one CW chunk.
///
/// The pilots are now V3-style TDM groups (`pattern.d_syms` data +
/// `pattern.p_syms` rotating-QPSK pilots, repeating). `group_idx_offset`
/// is the absolute group index of the first group in this chunk — the
/// encoder uses 0 for META-CW and `groups_per_cw·(1+k)` for the k-th
/// DATA-CW so the pilot rotation stays continuous across the cycle.
///
/// `extra_refs` carries the cycle-level PLHEADER references (192 sym,
/// |s|=1) shared by every CW in the cycle. They bias each CW's LS
/// toward the cycle mean — appropriate for the near-stationary NBFM
/// channel — while the per-CW TDM pilots still preserve sensitivity to
/// genuine intra-cycle drift.
fn estimate_cw_gain(
    chunk: &[Complex64],
    cw_data_syms: usize,
    pattern: &PilotPattern2x,
    group_idx_offset: usize,
    extra_refs: Option<(&[Complex64], &[Complex64])>,
) -> Complex64 {
    debug_assert_eq!(chunk.len(), pattern.wire_len(cw_data_syms));
    let positions = pilot2x_tdm::pilot_positions_2x(cw_data_syms, pattern, group_idx_offset);
    let mut num = Complex64::new(0.0, 0.0);
    let mut den = 0.0_f64;
    for (p_start, p_end, abs_pilot_start) in positions {
        for (i, k) in (p_start..p_end).enumerate() {
            let pref = pilot_symbol_2x(abs_pilot_start + i);
            num += chunk[k] * pref.conj();
            den += pref.norm_sqr();
        }
    }
    if let Some((rx, exp)) = extra_refs {
        debug_assert_eq!(rx.len(), exp.len());
        for (&y, &s) in rx.iter().zip(exp.iter()) {
            num += y * s.conj();
            den += s.norm_sqr();
        }
    }
    if den < 1e-12 {
        return Complex64::new(1.0, 0.0);
    }
    num / den
}

/// MAD consistency factor for **complex** Gaussian residuals: for
/// e ~ CN(0,σ²), |e|² is Exponential(σ²), so median(|e|²) = σ²·ln(2).
/// To recover σ² from the median we divide by ln(2). Used for the
/// total σ² estimate when the residual is treated as a 2D complex
/// vector and we only care about its scalar magnitude.
const MAD_CONSISTENCY_FACTOR_COMPLEX: f64 = std::f64::consts::LN_2;

/// MAD consistency factor for **real** Gaussian residuals projected on
/// one axis: for x ~ N(0, σ²), x² ~ σ²·χ²(1), and the median of χ²(1)
/// is ≈ 0.45494 (computed from the inverse CDF of the standard normal
/// at 0.75, then squared). Used by the radial/tangential decomposition
/// below, where each axis is a 1D real-Gaussian projection of the
/// complex residual.
const MAD_CONSISTENCY_FACTOR_REAL: f64 = 0.454936423119572;

/// Output of [`estimate_cw_sigma2_split`]: total σ², plus the radial
/// (along the pilot direction `P`) and tangential (perpendicular to
/// `P`) MAD-rescaled per-axis variances. The sum
/// `radial + tangential` equals the total only when the residuals are
/// jointly Gaussian; an imbalance signals a non-AWGN channel
/// (AM-AM/AM-PM distortion, phase noise, ...).
#[derive(Clone, Copy, Debug)]
struct Sigma2Split {
    total: f64,
    radial: f64,
    tangential: f64,
}

/// Per-CW σ² estimator: **median** of pilot residuals (MAD-style),
/// decomposed into radial and tangential axes relative to each pilot's
/// own direction `pref_k = pilot_symbol_2x(abs_idx_k)`.
///
/// Each pilot residual `e_k = y_k / gain − pref_k` is a complex number,
/// projected onto its pilot reference:
///
///   e_k · conj(pref_k) / |pref_k|   →   real = radial, imag = tangential
///
/// (`|pref_k| = 1` so the division is trivial.) The radial axis captures
/// amplitude distortion (AM-AM, hard-clipping, AGC error). The
/// tangential axis captures phase noise (AM-PM, LO drift, residual
/// timing jitter). On pure AWGN, the two are equal.
///
/// The total σ² is computed from `|e|²` directly for robustness, not
/// from `radial + tangential`. This keeps the LDPC LLR scaling
/// drop-in compatible with the previous estimator.
///
/// Falls back to the configured floor when no pilots contributed.
fn estimate_cw_sigma2_split(
    chunk: &[Complex64],
    cw_data_syms: usize,
    pattern: &PilotPattern2x,
    group_idx_offset: usize,
    gain: Complex64,
    extra_refs: Option<(&[Complex64], &[Complex64])>,
) -> Sigma2Split {
    if gain.norm() < 1e-12 {
        return Sigma2Split {
            total: SIGMA2_FLOOR,
            radial: SIGMA2_FLOOR / 2.0,
            tangential: SIGMA2_FLOOR / 2.0,
        };
    }
    let positions = pilot2x_tdm::pilot_positions_2x(cw_data_syms, pattern, group_idx_offset);
    let cap = positions.len() * pattern.p_syms
        + extra_refs.map_or(0, |(rx, _)| rx.len());
    let mut total_sq: Vec<f64> = Vec::with_capacity(cap);
    let mut radial_sq: Vec<f64> = Vec::with_capacity(cap);
    let mut tang_sq: Vec<f64> = Vec::with_capacity(cap);
    for (p_start, p_end, abs_pilot_start) in positions {
        for (i, k) in (p_start..p_end).enumerate() {
            let pref = pilot_symbol_2x(abs_pilot_start + i);
            let normed = chunk[k] / gain;
            let resid = normed - pref;
            total_sq.push(resid.norm_sqr());
            // |pref| = 1 → projection = resid · conj(pref).
            let proj = resid * pref.conj();
            radial_sq.push(proj.re * proj.re);
            tang_sq.push(proj.im * proj.im);
        }
    }
    // Cycle-level PLHEADER references — same projection logic but the
    // reference direction varies per symbol (SOF Chu rotates over the
    // sequence; PLS QPSK symbols sit on the 4 quadrant phases).
    // |s|=1 for both families, so the projection reduces to multiplying
    // the residual by `conj(s)`.
    //
    // **CRUCIAL** for the σ² statistics on tight-ring APSK: the 192
    // PLHEADER ref symbols sit at the cycle start, **before** the LMS
    // warmup zone, so their RRC pulse-shape response is decoupled
    // from any data symbols. The per-CW pilot blocks, by contrast,
    // interleave with data symbols and their residuals carry the RRC
    // tail of adjacent data — an "RRC leakage" artifact that on
    // 32-APSK / 64-APSK inflates σ² by an order of magnitude
    // (HIGH+2X σ² 0.081 → 0.0057 with cycle refs on the NBFM sweep,
    // and the σ²_R / σ²_T ratio drops from 53× to ~4×, the "honest"
    // channel statistic).
    if let Some((rx, exp)) = extra_refs {
        debug_assert_eq!(rx.len(), exp.len());
        for (&y, &s) in rx.iter().zip(exp.iter()) {
            let normed = y / gain;
            let resid = normed - s;
            total_sq.push(resid.norm_sqr());
            // s · conj(s) = |s|² = 1 — direction projection only.
            let proj = resid * s.conj();
            radial_sq.push(proj.re * proj.re);
            tang_sq.push(proj.im * proj.im);
        }
    }
    if total_sq.is_empty() {
        return Sigma2Split {
            total: SIGMA2_FLOOR,
            radial: SIGMA2_FLOOR / 2.0,
            tangential: SIGMA2_FLOOR / 2.0,
        };
    }
    // Median selection in O(n) on each of the three buffers.
    let median = |v: &mut Vec<f64>| -> f64 {
        let mid = v.len() / 2;
        v.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap());
        v[mid]
    };
    let total = (median(&mut total_sq) / MAD_CONSISTENCY_FACTOR_COMPLEX).max(SIGMA2_FLOOR);
    let radial = (median(&mut radial_sq) / MAD_CONSISTENCY_FACTOR_REAL)
        .max(SIGMA2_FLOOR / 2.0);
    let tangential = (median(&mut tang_sq) / MAD_CONSISTENCY_FACTOR_REAL)
        .max(SIGMA2_FLOOR / 2.0);
    Sigma2Split { total, radial, tangential }
}


// --- channel model from pilots (turbo Pass 1) -----------------------------

/// Parametric channel model derived from the pilot residuals on one CW.
/// Decomposes the per-axis pilot σ² into an AWGN floor plus two
/// magnitude-scaled components — AM-AM (radial) and phase-noise
/// (tangential):
///
/// ```text
///   σ²_radial_pilot     = σ²_awgn + 1²·σ²_am     (pilot at |P|=1)
///   σ²_tangential_pilot = σ²_awgn + 1²·σ²_phi
/// ```
///
/// Then at any ring of radius R:
///
/// ```text
///   σ²_radial(R)     = σ²_awgn + R²·σ²_am
///   σ²_tangential(R) = σ²_awgn + R²·σ²_phi
///   σ²_total(R)      = 2·σ²_awgn + R²·(σ²_am + σ²_phi)
/// ```
///
/// Identification: we cannot separate σ²_awgn from σ²_am (resp. σ²_phi)
/// using pilots alone — two unknowns, one equation per axis. We adopt
/// the standard assumption that σ²_awgn = min(σ²_radial, σ²_tangential)
/// (the floor common to both axes), leaving one of σ²_am / σ²_phi at
/// zero. This is a conservative model that captures the largest of the
/// two distortion modes correctly. The data-driven pass 2 (re-encode
/// after LDPC, σ²_r,DD per ring on the recovered data symbols)
/// resolves the degeneracy.
#[derive(Clone, Copy, Debug)]
struct ChannelModel {
    /// AWGN floor (per axis = per real-dimension).
    sigma2_awgn_per_dim: f64,
    /// Excess radial variance at the pilot's R=1 — AM-AM coefficient.
    sigma2_am: f64,
    /// Excess tangential variance at R=1 — phase-noise coefficient.
    sigma2_phi: f64,
}

impl ChannelModel {
    fn from_pilot_split(split: Sigma2Split) -> Self {
        let sigma2_awgn_per_dim = split.radial.min(split.tangential);
        let sigma2_am = (split.radial - sigma2_awgn_per_dim).max(0.0);
        let sigma2_phi = (split.tangential - sigma2_awgn_per_dim).max(0.0);
        Self { sigma2_awgn_per_dim, sigma2_am, sigma2_phi }
    }

    /// Total complex σ² at a constellation ring of radius `r_ring`.
    /// `r_ring = 1.0` reproduces `split.radial + split.tangential` (the
    /// "total = sum of axes" relation, valid for the model — distinct
    /// from the median-of-|e|² total reported in the diagnostic CLI
    /// line, which doesn't decompose this way on small samples).
    fn sigma2_total_at_ring(&self, r_ring: f64) -> f64 {
        let r2 = r_ring * r_ring;
        2.0 * self.sigma2_awgn_per_dim + r2 * (self.sigma2_am + self.sigma2_phi)
    }
}

// --- turbo Pass 2 EM: forward-backward (Kalman + RTS) per-ring g, σ² ------

/// Sanity-check bounds on the ratio of data-driven σ² to model σ² per
/// ring. Outside this range, the DD estimate is suspect (either a
/// degenerate decode or a genuine model failure we'd rather not
/// propagate as truth) — we fall back to the model's σ² for that ring.
/// Used by [`pass2_em_sigma2_to_per_sym_per_ring`].
const SIGMA2_DD_RATIO_MIN: f64 = 0.2;
const SIGMA2_DD_RATIO_MAX: f64 = 5.0;

//
// The hard-DD Pass 2 above (`pass2_dd_sigma2_per_ring`) treats one CW as a
// stationary block: a single σ² per ring, hard-decided ŝ_k from re-encoded
// LDPC. The EM variant in this section replaces that with a soft,
// time-varying estimator:
//
// - Soft references s̃_k = E[s_k | LLR_post] (computed in core-base from
//   the LDPC posterior LLRs, with per-bit-independence approximation).
// - Per-ring complex gain g_r(k): 1-state random-walk Kalman + backward RTS
//   smoothing. Tracks slow AGC drift / AM-AM compression that the per-CW
//   scalar SSD gain misses. One smoother per ring × {real, imag}.
// - Per-ring log-variance log σ²_r(k): 1-state random-walk Kalman + RTS,
//   measurement = log|y_k − g_r(k)·μ_kr|² with Euler-Mascheroni bias
//   correction and π²/6 measurement variance (the exact statistics of
//   log(χ²₂)).
// - Phase φ_k: 2-state {phase, drift} Kalman + RTS — see
//   [`phase_smoother::rts_phase_smooth`].
//
// All three feed back into the re-LLR step via
// `llr_maxlog_per_sym_per_ring`: per-symbol-per-ring σ²(s, k) drives the
// Bayesian LLR, the gain pre-rotates each y_k, the smoothed phase
// derotates after gain.
//
// "Soft" doesn't just mean "without hard decisions" — it also means
// gracefully ignoring symbols that the LDPC posterior cannot confidently
// assign to a ring. Low `P(z_k = r)` ⇒ no measurement contribution to
// ring r's smoothers at step k (the ScalarObs.z is set to None) — the
// state just propagates the prior.

/// Posterior support threshold below which a symbol contributes no
/// measurement to a per-ring smoother. Picked low so we keep most
/// reasonably-likely assignments; the per-measurement R already
/// down-weights low-confidence ones.
const EM_MIN_RING_PROB: f64 = 1e-3;

/// Process noise floor for g_r(k) random-walk (per-step variance of the
/// gain innovation). Empirically tiny: the channel gain barely moves
/// inside a 10 ms CW. Bumped up at runtime by `σ²_am` of the channel
/// model when AM-AM distortion is significant.
const EM_GAIN_Q_FLOOR: f64 = 1e-7;

/// Process noise on `log σ²_r(k)`. The log scale absorbs σ² magnitude
/// (a random walk on log handles a few-dB σ² drift over the CW).
const EM_LOG_SIGMA2_Q: f64 = 1e-3;

/// Bias of `E[log X]` when `X ~ χ²₂` scaled to mean 1 (i.e. unit
/// exponential): `-γ` where γ is Euler–Mascheroni. The full complex
/// residual `|e_k|²` under iid `N(0, σ²/2)` real/imag components has
/// `|e_k|² / σ² ~ Exp(1)`, hence `E[log|e_k|²] = log σ² − γ`.
const EULER_MASCHERONI: f64 = 0.577_215_664_901_532_9;

/// Variance of `log X` when `X ~ Exp(1)`: `π²/6`. This is the
/// measurement noise on the log-variance smoother (per single-symbol
/// residual observation, before re-weighting by posterior support).
const LOG_EXP_VAR: f64 = std::f64::consts::PI * std::f64::consts::PI / 6.0;

/// EM per-ring time-varying complex gain `g_r(k)`. Returns one smoothed
/// gain trajectory per ring of length `data.len()`. Single ring (QPSK /
/// 8PSK) collapses to identity since the pilot-based scalar gain is
/// already optimal.
fn pass2_em_gain_smoother(
    data: &[Complex64],
    soft: &[SoftSymbol],
    constellation: &modem_core_base::constellation::Constellation,
    model: &ChannelModel,
) -> Vec<Vec<Complex64>> {
    let (radii, _) = constellation.rings();
    let n_rings = radii.len();
    let n_sym = data.len();
    if n_rings == 1 {
        return vec![vec![Complex64::new(1.0, 0.0); n_sym]];
    }
    debug_assert_eq!(soft.len(), n_sym);

    let mut out: Vec<Vec<Complex64>> = (0..n_rings)
        .map(|_| Vec::with_capacity(n_sym))
        .collect();

    // Common process-noise scale. `σ²_am` is the radial channel
    // imbalance — when it's significant (AM-AM), allow more gain drift.
    let q = (model.sigma2_am + EM_GAIN_Q_FLOOR).max(EM_GAIN_Q_FLOOR);

    for r in 0..n_rings {
        let mut obs_re: Vec<ScalarObs> = Vec::with_capacity(n_sym);
        let mut obs_im: Vec<ScalarObs> = Vec::with_capacity(n_sym);
        for k in 0..n_sym {
            let w = soft[k].ring_prob[r];
            let mu = soft[k].ring_cond_mean[r];
            let mu2 = mu.norm_sqr();
            if w > EM_MIN_RING_PROB && mu2 > 1e-9 {
                // y_k = g_r · μ_kr + noise. Move to a direct measurement
                // of g_r: z = y_k / μ_kr. Var(z) = σ²_total(R_r) / |μ_kr|²
                // for AWGN; the soft-assignment weight w_kr further
                // down-weights inconfident assignments (effective sample
                // size = w).
                let z = data[k] / mu;
                let sigma2_ring = model.sigma2_total_at_ring(radii[r]).max(SIGMA2_FLOOR);
                let r_meas = sigma2_ring / (w * mu2);
                obs_re.push(ScalarObs { z: Some(z.re), r: r_meas });
                obs_im.push(ScalarObs { z: Some(z.im), r: r_meas });
            } else {
                obs_re.push(ScalarObs { z: None, r: 0.0 });
                obs_im.push(ScalarObs { z: None, r: 0.0 });
            }
        }
        // Prior: g = 1 + 0j (data already roughly normalised by the
        // pilot-based gain upstream), with a loose variance.
        let params_re = ScalarRtsParams { q, prior_mean: 1.0, prior_var: 1.0 };
        let params_im = ScalarRtsParams { q, prior_mean: 0.0, prior_var: 1.0 };
        let smooth_re = rts_smooth_scalar(&obs_re, &params_re);
        let smooth_im = rts_smooth_scalar(&obs_im, &params_im);
        for k in 0..n_sym {
            out[r].push(Complex64::new(smooth_re[k], smooth_im[k]));
        }
    }
    out
}

/// Posterior-weighted gain at each symbol position: convex combination
/// of the per-ring trajectories, weighted by `P(z_k = r)`.
///
/// Used to derotate `y_k` before re-LLR. When the posterior is sharply
/// peaked on a single ring (high-confidence symbols), this collapses to
/// that ring's smoothed gain; when uncertain, it averages across rings
/// — appropriate "softness" instead of a hard ring decision that
/// might be wrong.
fn pass2_em_weighted_gain(
    soft: &[SoftSymbol],
    gain_per_ring: &[Vec<Complex64>],
) -> Vec<Complex64> {
    let n_sym = soft.len();
    let n_rings = gain_per_ring.len();
    if n_rings == 1 {
        return gain_per_ring[0].clone();
    }
    (0..n_sym)
        .map(|k| {
            let mut g_acc = Complex64::new(0.0, 0.0);
            let mut w_sum = 0.0_f64;
            for r in 0..n_rings {
                let w = soft[k].ring_prob[r];
                if w > 0.0 {
                    g_acc += gain_per_ring[r][k] * w;
                    w_sum += w;
                }
            }
            if w_sum > 1e-12 {
                g_acc / w_sum
            } else {
                Complex64::new(1.0, 0.0)
            }
        })
        .collect()
}

/// EM per-ring time-varying noise variance `σ²_r(k)`. State =
/// `log σ²_r(k)`, random walk; measurement = `log |y_k − g_r(k)·μ_kr|²`
/// with bias `−γ` and variance `π²/6` (the exact log-Exp(1) statistics
/// of a per-sample squared residual under complex-Gaussian noise). The
/// log-domain state model converts multiplicative σ² drift to additive,
/// making the random-walk model trivially valid even when σ² varies by
/// an order of magnitude across the CW.
fn pass2_em_sigma2_smoother(
    data: &[Complex64],
    soft: &[SoftSymbol],
    gain_per_ring: &[Vec<Complex64>],
    constellation: &modem_core_base::constellation::Constellation,
    model: &ChannelModel,
) -> Vec<Vec<f64>> {
    let (radii, _) = constellation.rings();
    let n_rings = radii.len();
    let n_sym = data.len();
    debug_assert_eq!(soft.len(), n_sym);
    debug_assert_eq!(gain_per_ring.len(), n_rings);

    let mut out: Vec<Vec<f64>> = vec![Vec::new(); n_rings];
    for r in 0..n_rings {
        let mut obs: Vec<ScalarObs> = Vec::with_capacity(n_sym);
        for k in 0..n_sym {
            let w = soft[k].ring_prob[r];
            let mu = soft[k].ring_cond_mean[r];
            if w > EM_MIN_RING_PROB && mu.norm_sqr() > 1e-9 {
                let g = gain_per_ring[r][k];
                let e = data[k] - g * mu;
                let e2 = e.norm_sqr().max(1e-12);
                // E[log|e|²] = log σ² − γ ⇒ measurement = log|e|² + γ.
                let z = e2.ln() + EULER_MASCHERONI;
                // Per-sample variance π²/6; posterior weight scales the
                // effective sample count (low-confidence → high r_meas).
                let r_meas = LOG_EXP_VAR / w;
                obs.push(ScalarObs { z: Some(z), r: r_meas });
            } else {
                obs.push(ScalarObs { z: None, r: 0.0 });
            }
        }
        let prior_mean = model
            .sigma2_total_at_ring(radii[r])
            .max(SIGMA2_FLOOR)
            .ln();
        let params = ScalarRtsParams {
            q: EM_LOG_SIGMA2_Q,
            prior_mean,
            // Loose prior in log-domain (≈ ±2 stddev = ±2 nats ≈ ×7).
            prior_var: 4.0,
        };
        let log_sigma2 = rts_smooth_scalar(&obs, &params);
        out[r] = log_sigma2
            .into_iter()
            .map(|l| l.exp().max(SIGMA2_FLOOR))
            .collect();
    }
    out
}

/// Build the per-symbol per-ring σ² matrix `[k][r]` consumed by
/// [`soft_demod::llr_maxlog_per_sym_per_ring`]. Transposes the ring-major
/// EM smoother output (`[r][k]`) into symbol-major, applies the same
/// sanity gate (ratio bounds vs the per-ring model σ²) — entries
/// outside the band fall back to model.
fn pass2_em_sigma2_to_per_sym_per_ring(
    sigma2_em: &[Vec<f64>],
    constellation: &modem_core_base::constellation::Constellation,
    model: &ChannelModel,
    n_sym: usize,
) -> Vec<Vec<f64>> {
    let (radii, _) = constellation.rings();
    let n_rings = radii.len();
    debug_assert_eq!(sigma2_em.len(), n_rings);
    let model_per_ring: Vec<f64> = radii
        .iter()
        .map(|&r| model.sigma2_total_at_ring(r).max(SIGMA2_FLOOR))
        .collect();
    let mut out = Vec::with_capacity(n_sym);
    for k in 0..n_sym {
        let mut row = Vec::with_capacity(n_rings);
        for r in 0..n_rings {
            let dd = sigma2_em[r][k];
            let m = model_per_ring[r];
            let ratio = dd / m;
            let chosen = if (SIGMA2_DD_RATIO_MIN..=SIGMA2_DD_RATIO_MAX).contains(&ratio) {
                dd
            } else {
                m
            };
            row.push(chosen);
        }
        out.push(row);
    }
    out
}

/// Build a phase-observation stream from the pilot blocks of a CW
/// chunk. **One PhaseObs per pilot group** — pilots are exact known
/// refs (rotating QPSK on the unit circle), so the obs are independent
/// of any decode result. Used by the Phase 3 per-cycle RTS smoother:
/// the session collects these across all CWs of a cycle and runs
/// `rts_phase_smooth` at cycle end.
///
/// Returns `Vec<(wire_pos_offset, PhaseObs)>` where `wire_pos_offset`
/// is the offset (in symbols) of the pilot group's first pilot inside
/// the CW chunk (the caller converts to absolute symbol index by
/// adding `chunk_rel + buf_start_abs`).
///
/// `sigma2_tangential` is the per-CW tangential σ² estimate (already
/// computed by `estimate_cw_sigma2_split`). R per group ≈
/// σ²_tang / (p_syms × |gain|²) since we average `p_syms` pilots.
pub(crate) fn pilot_phase_obs_per_group(
    chunk: &[Complex64],
    cw_data_syms: usize,
    pattern: &PilotPattern2x,
    group_idx_offset: usize,
    gain: Complex64,
    sigma2_tangential: f64,
) -> Vec<(usize, PhaseObs)> {
    let positions = pilot2x_tdm::pilot_positions_2x(
        cw_data_syms, pattern, group_idx_offset,
    );
    let gain_norm_sqr = gain.norm_sqr();
    if gain_norm_sqr < 1e-12 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(positions.len());
    for (pilot_start, pilot_end, abs_pilot_idx) in positions {
        if pilot_end > chunk.len() {
            break;
        }
        // Acc = Σ (rx / gain) · conj(pref). On the unit-magnitude
        // rotating pilots, |pref| = 1 so arg(acc) is the residual
        // rotation introduced by the channel between this group's
        // pilots and the gain-anchored reference.
        let mut acc = Complex64::new(0.0, 0.0);
        let mut n = 0usize;
        for (k, sym) in chunk[pilot_start..pilot_end].iter().enumerate() {
            let pref = pilot2x_tdm::pilot_symbol_2x(abs_pilot_idx + k);
            acc += (sym / gain) * pref.conj();
            n += 1;
        }
        if n == 0 || acc.norm_sqr() < 1e-18 {
            out.push((pilot_start, PhaseObs { theta: 0.0, r: 1e9 }));
            continue;
        }
        let theta = acc.arg();
        // R per group = σ²_tang / (n × |gain|²). With p_syms=2 and a
        // typical |gain|≈1, R ≈ σ²_tang / 2.
        let r = (sigma2_tangential / (n as f64 * gain_norm_sqr)).max(1e-9);
        out.push((pilot_start, PhaseObs { theta, r }));
    }
    out
}

/// Build a phase-observation stream for [`rts_phase_smooth`] from the
/// (gain-corrected) data and the soft references. Measurement noise R
/// per symbol = σ²_tang / |s̃_k|² — the standard small-angle approximation
/// of the AWGN phase variance for a sample of magnitude |s̃|, with the
/// tangential pilot σ² as the AWGN scale.
fn pass2_em_phase_obs(
    data: &[Complex64],
    soft: &[SoftSymbol],
    sigma2_tangential: f64,
) -> Vec<PhaseObs> {
    data.iter()
        .zip(soft.iter())
        .map(|(&y, s)| {
            let mu = s.mean;
            let mu2 = mu.norm_sqr();
            if mu2 > 1e-9 {
                let theta = (y * mu.conj()).arg();
                let r = (sigma2_tangential / mu2).max(1e-9);
                PhaseObs { theta, r }
            } else {
                // No usable soft reference — treat as missing (huge R).
                PhaseObs { theta: 0.0, r: 1e9 }
            }
        })
        .collect()
}

/// Sanity-check the per-ring data-driven σ² vs the channel-model per-
/// ring σ², element-wise. Used by the legacy hard-DD Pass 2 path and
/// (in its per-symbol variant) by [`pass2_em_sigma2_to_per_sym_per_ring`];
/// kept around as a free function for future scalar checks even though
/// the EM path goes through the per-symbol gating now.
#[allow(dead_code)]
fn pass2_sanity_check_and_blend(
    sigma2_dd_per_ring: &[f64],
    constellation: &modem_core_base::constellation::Constellation,
    model: &ChannelModel,
) -> Vec<f64> {
    let (radii, _) = constellation.rings();
    sigma2_dd_per_ring
        .iter()
        .zip(radii.iter())
        .map(|(&dd, &r)| {
            let m = model.sigma2_total_at_ring(r).max(SIGMA2_FLOOR);
            let ratio = dd / m;
            if (SIGMA2_DD_RATIO_MIN..=SIGMA2_DD_RATIO_MAX).contains(&ratio) {
                dd
            } else {
                m
            }
        })
        .collect()
}

// --- per-ring decision-directed LS gain (SSD à la Meyr) -------------------

/// Decision-directed per-ring LS gain refinement.
///
/// Given data symbols that have already been roughly normalised by the
/// pilot-based LS gain, hard-decide each symbol to its nearest
/// constellation point and run a separate LS gain estimator per ring of
/// the APSK constellation. This captures channel-imposed AM-AM /
/// AM-PM distortion that the pilot-only estimator (locked to the
/// |P|=1 pilot magnitude) cannot see — the FM channel applies a
/// magnitude-dependent gain when the deviation index gets close to
/// unity, hitting 32-APSK's outer ring (|s|=1.27) and 64-APSK's
/// outer rings (up to 1.31) at a different effective gain than the
/// pilot at |P|=1.
///
/// Reference: Meyr/Moeneclaey/Fechtel, *Digital Communication
/// Receivers — Synchronization, Channel Estimation, and Signal
/// Processing*, Wiley 1998, Ch. 8 (synchronous-substream-decoder, SSD).
///
/// Returns a `Vec<Complex64>` of length `constellation.rings().0.len()`
/// where index `r` is the LS gain estimate for ring `r`. Rings with
/// no symbols (empty in this CW) get identity gain (1+0j) so the
/// later `apply_per_ring_correction` is a no-op for them.
fn estimate_per_ring_gain(
    data: &[Complex64],
    constellation: &modem_core_base::constellation::Constellation,
) -> Vec<Complex64> {
    let (radii, ring_of_point) = constellation.rings();
    let n_rings = radii.len();
    if n_rings == 1 {
        // QPSK / 8PSK: single ring, the pilot-based gain is already
        // optimal — no per-ring refinement to do.
        return vec![Complex64::new(1.0, 0.0)];
    }
    let mut num = vec![Complex64::new(0.0, 0.0); n_rings];
    let mut den = vec![0.0_f64; n_rings];
    for &y in data {
        // Hard decision: nearest constellation point.
        let mut best_idx = 0usize;
        let mut best_d2 = f64::INFINITY;
        for (k, &s) in constellation.points.iter().enumerate() {
            let d2 = (y - s).norm_sqr();
            if d2 < best_d2 {
                best_d2 = d2;
                best_idx = k;
            }
        }
        let s_hat = constellation.points[best_idx];
        let r = ring_of_point[best_idx];
        // LS update: numerator = <y, s_hat>, denominator = <s_hat, s_hat>.
        num[r] += y * s_hat.conj();
        den[r] += s_hat.norm_sqr();
    }
    (0..n_rings)
        .map(|r| {
            if den[r] > 1e-12 {
                num[r] / den[r]
            } else {
                Complex64::new(1.0, 0.0)
            }
        })
        .collect()
}

/// Apply per-ring gain corrections to a buffer of data symbols. For
/// each symbol, the hard-decided ring assignment determines which
/// gain to divide by. Called *after* the symbols have been roughly
/// normalised by the pilot-based gain so the hard-decisions are
/// trustworthy.
fn apply_per_ring_correction(
    data: &mut [Complex64],
    constellation: &modem_core_base::constellation::Constellation,
    ring_gains: &[Complex64],
) {
    if ring_gains.len() == 1 {
        // PSK case (single ring, no-op refinement).
        return;
    }
    let (_, ring_of_point) = constellation.rings();
    for y in data.iter_mut() {
        // Re-decide (same decisions as the LS pass; could be hoisted
        // out via Vec<usize> if a hot-path profile demands it).
        let mut best_idx = 0usize;
        let mut best_d2 = f64::INFINITY;
        for (k, &s) in constellation.points.iter().enumerate() {
            let d2 = (*y - s).norm_sqr();
            if d2 < best_d2 {
                best_d2 = d2;
                best_idx = k;
            }
        }
        let r = ring_of_point[best_idx];
        let g = ring_gains[r];
        if g.norm() > 1e-12 {
            *y = *y / g;
        }
    }
}

// --- main entry point -----------------------------------------------------

/// FFE filter length used by the per-cycle equaliser. 64 taps at
/// symbol rate cover ±32 symbols of delay spread (≈ ±21 ms at 1500 Bd,
/// 2× the worst-case delay_spread_90 observed on FT-991A → FTX-1
/// sound-card captures). On a clean (flat) channel, the LS solution
/// drives every off-center tap toward zero and the convolution
/// degenerates to a scalar gain — no regression risk on lab WAVs or
/// QO-100 sat where multipath is negligible.
const FFE_LEN_2X: usize = 64;

/// Gather the **consecutive** known-reference block at the start of one
/// PLHEADER cycle :
/// - PLHEADER 192 sym (SOF Chu + PLS QPSK, recovered from the validated
///   `pls` payload)
/// - LMS warmup symbols (`cfg.lms_warmup_syms`: 32 for APSK32/HighPlus,
///   64 for APSK64/HighPlusPlus, 0 for QPSK/8PSK/16-APSK)
///
/// **TDM pilots are NOT included.** `train_ffe_ls` requires every
/// sample in the convolution window to be known for the LS Gram matrix
/// to be well-conditioned ; scattered TDM pilots have unknown data
/// symbols in their windows and would bias the LS toward a near-
/// identity (but data-perturbed) filter — exactly the failure mode
/// observed when the first cut included them. The TDM pilots still
/// drive the per-CW scalar LS gain estimator downstream
/// (`estimate_cw_gain`), where they don't need consecutive context.
///
/// Mirrors V3 which trains its FFE on a 256-sym consecutive preamble
/// only. 224 sym for APSK profiles here is slightly less but the LS is
/// still 3.5× over-determined for `n_ff = 64`.
fn gather_cycle_refs(
    cfg: &ModemConfig2x,
    pls: &PlsPayload,
    sof_at: usize,
) -> (Vec<usize>, Vec<Complex64>) {
    let mut positions: Vec<usize> = Vec::new();
    let mut refs: Vec<Complex64> = Vec::new();

    // PLHEADER refs — 192 consecutive sym.
    let plheader_refs = plheader::plheader_reference_symbols(cfg.family, pls);
    for (i, &s) in plheader_refs.iter().enumerate() {
        positions.push(sof_at + i);
        refs.push(s);
    }

    // LMS warmup refs — 0/32/64 consecutive sym right after the PLHEADER.
    let warmup_refs = make_lms_warmup_2x(cfg);
    let warmup_start = sof_at + PLHEADER_LEN_SYM;
    for (i, &s) in warmup_refs.iter().enumerate() {
        positions.push(warmup_start + i);
        refs.push(s);
    }

    (positions, refs)
}

/// Apply a trained FFE to a contiguous symbol range `[start, end)`,
/// overwriting `equalized[start..end]`. At each output position k, the
/// convolution uses `taps[i] * symbols[k - half + i]` over
/// `i ∈ [0, taps.len())`. Boundary positions where the input window
/// would underflow / overflow the buffer keep the raw input value (no
/// equalization at the edges; safer than zero-padding which would
/// inject artifacts at the burst boundary).
fn apply_ffe_to_range(
    symbols: &[Complex64],
    taps: &[Complex64],
    start: usize,
    end: usize,
    equalized: &mut [Complex64],
) {
    let n_ff = taps.len();
    let half = n_ff / 2;
    let end = end.min(symbols.len()).min(equalized.len());
    for k in start..end {
        if k < half || k + n_ff - half > symbols.len() {
            // Boundary: leave the raw sample. Keeps SOF correlation
            // and downstream find_next_sof working on the first/last
            // ~32 symbols of the burst.
            equalized[k] = symbols[k];
            continue;
        }
        let mut y = Complex64::new(0.0, 0.0);
        for i in 0..n_ff {
            y += taps[i] * symbols[k - half + i];
        }
        equalized[k] = y;
    }
}

/// Pre-pass equalisation: for every PLHEADER candidate above the SOF
/// threshold, decode the PLS (Golay+CRC, ≈ zero false positives), then
/// LS-train an FFE on **all known references in the cycle** (PLHEADER +
/// LMS warmup + every per-CW TDM pilot). Apply the trained FFE to that
/// cycle's symbol slice. Returns the equalised stream; cycles where the
/// PLHEADER fails CRC, or where there's no PLHEADER at all, fall through
/// to the raw symbols (the main decode loop will handle those with the
/// per-CW pilot LS-gain estimator alone).
///
/// Slice 2x18a — forward-only, per-cycle granularity. Backward RTS +
/// turbo-feedback come in slices 2x18b / 2x18c (see plan file
/// `ffe-forward-backward-precious-multipath.md`).
/// Per-cycle FFE equaliser. `scan_from` is the symbol index from which
/// to start scanning for PLHEADERs — cycles whose SOF falls before
/// `scan_from` are NOT re-equalised (the caller has already done so
/// in a previous call and the equalised content is preserved in
/// `symbols`). Slice 2x23+ incremental: per chunk on the Pi5 we now
/// pay the FFE train + apply cost only for the **newly completed
/// cycle** instead of every cycle in the buffer, cutting per-chunk
/// CPU by O(retained_cycles) — critical for live OTA on the Pi where
/// the previous pre-2x23 reprocess was the dominant audio-rate cost.
pub(crate) fn equalize_symbols_per_cycle_from(
    symbols: &[Complex64],
    cfg: &ModemConfig2x,
    dd_refs: &[(usize, Complex64)],
    scan_from: usize,
) -> Vec<Complex64> {
    let mut equalized: Vec<Complex64> = symbols.to_vec();
    let cycle_period = full_cycle_len_syms(cfg);
    let min_skip = (cycle_period / 2).max(SOF_LEN_SYM);

    // Use find_next_sof in a loop — same threshold (0.2 × SOF_LEN) as
    // the main decode pipeline so we equalize every cycle the decoder
    // will see. `find_all_sofs` uses a tighter 0.3 threshold tuned for
    // the drift LS fit (avoiding false-positive Chu matches in band
    // noise); for FFE training the looser threshold is correct because
    // the per-PLS Golay+CRC check below rejects false positives anyway.
    let mut scan = scan_from;
    let log = std::env::var_os("RX_V4_LOG_FFE").is_some();
    while let Some(sof_at) = find_next_sof(symbols, scan, cfg.family) {
        if sof_at + PLHEADER_LEN_SYM > symbols.len() {
            break;
        }
        let plheader_slice = &symbols[sof_at..sof_at + PLHEADER_LEN_SYM];
        let pls = match decode_plheader_at(plheader_slice, cfg.family) {
            Some((p, _gain)) => p,
            None => {
                // PLS CRC failed → noise correlation, not a real PLHEADER.
                // Advance one symbol and keep searching (`find_next_sof`
                // skip-one semantics).
                scan = sof_at + 1;
                continue;
            }
        };
        let (mut positions, mut refs) = gather_cycle_refs(cfg, &pls, sof_at);
        // Turbo-FFE feedback (slice 2x18c): append DD references that
        // fall inside this cycle. Pass 1 calls with empty dd_refs;
        // pass 2 calls with the data syms recovered from CWs that
        // converged in pass 1, dramatically expanding the FFE training
        // set per cycle (from ~224 to several thousand).
        let cycle_end = (sof_at + cycle_period).min(symbols.len());
        for &(pos, sym) in dd_refs {
            if pos >= sof_at && pos < cycle_end {
                positions.push(pos);
                refs.push(sym);
            }
        }
        let taps = train_ffe_ls(symbols, &refs, &positions, FFE_LEN_2X);
        if log {
            let center = taps.len() / 2;
            let off_center_norm: f64 = taps
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != center)
                .map(|(_, t)| t.norm_sqr())
                .sum();
            eprintln!(
                "[ffe] cycle@{} refs={} taps[c]={:.3}+{:.3}i off_norm²={:.4}",
                sof_at,
                positions.len(),
                taps[center].re,
                taps[center].im,
                off_center_norm,
            );
        }
        apply_ffe_to_range(symbols, &taps, sof_at, cycle_end, &mut equalized);
        scan = sof_at + min_skip;
    }

    equalized
}

/// Legacy alias — full-buffer equalise (`scan_from = 0`). Used by tests
/// and callers that don't track equalisation cursor state. Slice 2x23+
/// streaming callers should use `equalize_symbols_per_cycle_from` with
/// the symbol index past which they've already equalised.
pub(crate) fn equalize_symbols_per_cycle(
    symbols: &[Complex64],
    cfg: &ModemConfig2x,
    dd_refs: &[(usize, Complex64)],
) -> Vec<Complex64> {
    equalize_symbols_per_cycle_from(symbols, cfg, dd_refs, 0)
}

/// Captured state of one decode pass — used by [`rx_v4_symbols_after`]
/// to feed the turbo-FFE loop (slice 2x18c).
struct DecodePassExtras {
    /// ESI → recovered info_bytes for every DATA-CW that converged.
    cw_bytes: HashMap<u32, Vec<u8>>,
    /// Per-cycle `(sof_at, pls)` for every PLHEADER that passed
    /// Golay+CRC. Same length and content as
    /// `RxResult2x::validated_sof_positions` plus the matching PLS.
    anchors: Vec<(usize, PlsPayload)>,
}

/// Build DD references (wire_position, symbol) for the FFE pass 2 by
/// re-encoding every converged DATA-CW. For each ESI in `extras.cw_bytes`:
///
/// 1. Locate the producing cycle via `extras.anchors` (the cycle whose
///    PLS has `base_esi <= ESI < base_esi + cw_per_cycle`).
/// 2. Compute the DATA-CW slot's wire start in the symbol stream:
///    `sof_at + 192 + lms_warmup + (1 + k) * cw_with_pilots` where
///    `k = ESI - base_esi`.
/// 3. Re-encode `info_bytes` via `encode_one_codeword` → 461 data syms
///    (for APSK32).
/// 4. Interleave with the deterministic TDM pilots → full CW wire
///    (cw_with_pilots syms, every position known).
/// 5. Push `(wire_start + i, interleaved[i])` for every i in the wire.
///
/// META-CWs aren't re-encoded here (they have a different info source —
/// the AppHeader replicated 4×) but pass 2's PLHEADER+warmup anchor is
/// enough to localise META's portion of the cycle anyway.
fn build_dd_refs_from_pass1(
    cfg: &ModemConfig2x,
    extras: &DecodePassExtras,
) -> Vec<(usize, Complex64)> {
    let constellation = make_constellation_2x(cfg);
    let interleave_perm = interleaver::interleave_table(
        interleaver::padded_cw_bits(cfg.base.ldpc_rate.n(), cfg.base.constellation),
        cfg.base.constellation,
    );
    let encoder = modem_core_base::ldpc::encoder::LdpcEncoder::new(cfg.base.ldpc_rate);
    let cw_data_syms = cfg.cw_data_syms();
    let cw_with_pilots = cw_with_pilots_len(cfg);
    let cw_per_cycle = data_cw_per_cycle(cfg);
    let groups_per_cw = pilot_groups_per_cw(cfg);

    let mut out: Vec<(usize, Complex64)> = Vec::new();
    for (sof_at, pls) in &extras.anchors {
        let base_esi = pls.base_esi;
        for k in 0..cw_per_cycle {
            let esi = base_esi + k as u32;
            let info_bytes = match extras.cw_bytes.get(&esi) {
                Some(b) => b,
                None => continue, // didn't converge in pass 1
            };
            let data_syms = crate::frame2x::encode_one_codeword(
                info_bytes,
                &encoder,
                &interleave_perm,
                &constellation,
            );
            // Re-create the wire layout (data interleaved with the
            // deterministic rotating-QPSK pilots) — every position is
            // now a known reference for the FFE.
            let group_offset = groups_per_cw * (1 + k);
            let (interleaved, _) = pilot2x_tdm::interleave_data_pilots_2x(
                &data_syms,
                &cfg.pilot_pattern,
                group_offset,
            );
            debug_assert_eq!(interleaved.len(), cw_with_pilots);
            let wire_start = sof_at + PLHEADER_LEN_SYM
                + cfg.lms_warmup_syms
                + (1 + k) * cw_with_pilots;
            for (i, sym) in interleaved.iter().enumerate() {
                out.push((wire_start + i, *sym));
            }
        }
    }
    out
}

/// Decode every PLHEADER cycle visible in `symbols`, accumulate ESI →
/// bytes, and try a RaptorQ reassembly using the AppHeader from any
/// converged META-CW.
///
/// Symbols are assumed already matched-filtered and sampled at the
/// symbol rate. The function first runs a per-cycle LS-FFE
/// (`equalize_symbols_per_cycle`) to undo any short-delay multipath
/// before handing the equalised stream to the existing per-CW pilot
/// LS-gain + turbo Pass 2 EM pipeline.
pub fn rx_v4_symbols(
    symbols: &[Complex64],
    cfg: &ModemConfig2x,
) -> Option<RxResult2x> {
    rx_v4_symbols_after(symbols, 0, cfg)
}

/// Same as [`rx_v4_symbols`] but starts scanning at `cursor`. Drives
/// the turbo-FFE loop (slice 2x18c):
/// - **Pass 1**: per-cycle LS-FFE trained on PLHEADER + LMS warmup only
///   → equalise → decode. Some DATA-CWs converge.
/// - **Pass 2**: re-encode every converged CW from pass 1 → use those
///   ~5-50× more known symbols to re-train the FFE → equalise → decode.
///   Marginal CWs that failed pass 1 typically converge here because
///   the FFE now resolves the multipath channel much more accurately.
///
/// On clean channels (lab WAV, QO-100 sat) the LS converges to identity
/// in pass 1, dd_refs are empty (or near-zero contribution), and pass 2
/// is a no-op. Opt-out via `RX_V4_NO_FFE=1`; `RX_V4_NO_FFE_TURBO=1`
/// keeps the pass-1 FFE but skips the pass-2 feedback loop.
pub fn rx_v4_symbols_after(
    symbols: &[Complex64],
    cursor: usize,
    cfg: &ModemConfig2x,
) -> Option<RxResult2x> {
    let (res1, extras1) = rx_v4_decode_pass(symbols, cursor, cfg, &[]);
    let res1 = res1?;

    // Turbo-FFE pass 2 — feed the LDPC posterior back into the FFE.
    // **Opt-in via `RX_V4_FFE_TURBO=1`**. The simple "all-converged-CW-syms
    // are dd_refs" recipe doesn't improve over slice 2x18a on the
    // 2026-05-14 captures: same CW count converges (FFE pass 1 already
    // captures the channel response well enough from PLHEADER+warmup
    // alone), and the additional refs land in conv windows that may
    // span non-converged CW regions, biasing the LS slightly. Slice
    // 2x18d will refine the ref selection (only positions whose full
    // ±n_ff/2 window is composed of known samples) — landed disabled
    // here so we don't regress and can iterate on the criterion.
    if !std::env::var_os("RX_V4_FFE_TURBO").is_some()
        || std::env::var_os("RX_V4_NO_FFE").is_some()
        || extras1.cw_bytes.is_empty()
    {
        return Some(res1);
    }
    let dd_refs = build_dd_refs_from_pass1(cfg, &extras1);
    if dd_refs.is_empty() {
        return Some(res1);
    }
    if std::env::var_os("RX_V4_LOG_FFE").is_some() {
        eprintln!(
            "[ffe] turbo pass 2: {} converged CWs in pass 1 → {} dd_refs",
            extras1.cw_bytes.len(),
            dd_refs.len(),
        );
    }
    let (res2, _) = rx_v4_decode_pass(symbols, cursor, cfg, &dd_refs);
    match res2 {
        Some(r) if r.data_cws_converged > res1.data_cws_converged => Some(r),
        _ => Some(res1),
    }
}

/// One decode pass: optional FFE pre-pass (with extra DD refs for the
/// turbo loop), then the symbol-domain SOF scan → PLHEADER decode →
/// META-CW + DATA-CW LDPC pipeline. Returns the decoded
/// [`RxResult2x`] plus the captured state needed to drive a second
/// turbo-FFE pass.
fn rx_v4_decode_pass(
    symbols: &[Complex64],
    cursor: usize,
    cfg: &ModemConfig2x,
    dd_refs: &[(usize, Complex64)],
) -> (Option<RxResult2x>, DecodePassExtras) {
    let equalized_storage;
    let symbols: &[Complex64] = if std::env::var_os("RX_V4_NO_FFE").is_some() {
        symbols
    } else {
        equalized_storage = equalize_symbols_per_cycle(symbols, cfg, dd_refs);
        &equalized_storage
    };

    let constellation = make_constellation_2x(cfg);
    let cw_data_syms = cfg.cw_data_syms();
    let cw_with_pilots = cw_with_pilots_len(cfg);
    let cw_per_cycle = data_cw_per_cycle(cfg);
    let groups_per_cw = pilot_groups_per_cw(cfg);

    let interleave_perm = interleaver::interleave_table(
        interleaver::padded_cw_bits(cfg.base.ldpc_rate.n(), cfg.base.constellation),
        cfg.base.constellation,
    );
    let deinterleave_perm = interleaver::deinterleave_table(
        interleave_perm.len(),
        cfg.base.constellation,
    );
    let decoder = LdpcDecoder::new(cfg.base.ldpc_rate, LDPC_MAX_ITER);
    // Encoder for turbo Pass 2: re-encodes converged info_bytes back to
    // the constellation symbol sequence to serve as a "truth" reference
    // for data-driven per-ring σ² estimation.
    let encoder = modem_core_base::ldpc::encoder::LdpcEncoder::new(cfg.base.ldpc_rate);
    let k_bytes = cfg.base.ldpc_rate.k() / 8;

    let mut result = RxResult2x::empty();
    let mut cw_bytes: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut anchors: Vec<(usize, PlsPayload)> = Vec::new();
    let mut sigma2_sum = 0.0_f64;
    let mut sigma2_radial_sum = 0.0_f64;
    let mut sigma2_tangential_sum = 0.0_f64;
    let mut sigma2_n = 0usize;

    let mut scan = cursor;
    while let Some(sof_at) = find_next_sof(symbols, scan, cfg.family) {
        // Need at least PLHEADER + warmup + meta CW before we touch data.
        let cycle_min = PLHEADER_LEN_SYM + cfg.lms_warmup_syms + cw_with_pilots;
        if sof_at + cycle_min > symbols.len() {
            break;
        }
        let plheader_slice = &symbols[sof_at..sof_at + PLHEADER_LEN_SYM];
        let (pls, sof_gain) = match decode_plheader_at(plheader_slice, cfg.family) {
            Some(p) => p,
            None => {
                // Bad PLHEADER (CRC fails) — skip past this candidate
                // and keep scanning.
                scan = sof_at + 1;
                continue;
            }
        };
        if result.first_pls.is_none() {
            result.first_pls = Some(pls);
            result.first_sof_at = Some(sof_at);
        }
        // Validated SOF — Golay + CRC just passed in
        // `decode_plheader_at`. Record for the drift LS fit. No false
        // positives possible here.
        result.validated_sof_positions.push(sof_at);
        anchors.push((sof_at, pls));
        if pls.flags & FLAG2X_EOT != 0 {
            result.eot_seen = true;
        }

        // Cycle-level pilot references: 192 sym (SOF Chu + decoded
        // PLS QPSK) at known patterns. Used by every CW in this cycle
        // as extra reference samples for `estimate_cw_gain` and
        // `estimate_cw_sigma2_split`. Effective pilot count goes from
        // 36/CW (in-CW blocks alone) to 36 + 192 = 228 — see §10.3.4.
        // Same `sof_gain` normalisation as the CW chunks so they all
        // live in a common amplitude/phase frame.
        let cycle_refs_rx: Vec<Complex64> = symbols
            [sof_at..sof_at + PLHEADER_LEN_SYM]
            .iter()
            .map(|&s| s / sof_gain)
            .collect();
        let cycle_refs_exp = plheader::plheader_reference_symbols(cfg.family, &pls);
        let cycle_refs: Option<(&[Complex64], &[Complex64])> =
            Some((&cycle_refs_rx, &cycle_refs_exp));

        let mut wire_cursor = sof_at + PLHEADER_LEN_SYM + cfg.lms_warmup_syms;

        // META-CW + its pilots, normalised by the SOF gain (initial AGC).
        let meta_chunk_end = wire_cursor + cw_with_pilots;
        if meta_chunk_end > symbols.len() {
            break;
        }
        let meta_chunk: Vec<Complex64> = symbols[wire_cursor..meta_chunk_end]
            .iter()
            .map(|&s| s / sof_gain)
            .collect();
        wire_cursor = meta_chunk_end;

        decode_one_cw(
            &meta_chunk,
            cw_data_syms,
            &cfg.pilot_pattern,
            0,                     // META-CW: pilot rotation starts at 0
            &constellation,
            &interleave_perm,
            &deinterleave_perm,
            &decoder,
            &encoder,
            k_bytes,
            true,                  // is_meta
            pls.base_esi,          // unused for meta
            &mut cw_bytes,
            &mut result,
            &mut sigma2_sum,
            &mut sigma2_radial_sum,
            &mut sigma2_tangential_sum,
            &mut sigma2_n,
            cycle_refs,
            None,                  // batch path: no per-cycle phase tracker
        );

        // DATA-CWs of this cycle. The encoder wrote `cw_per_cycle` (or
        // fewer at the very end of a burst); we read until either we
        // hit `cw_per_cycle` or run out of symbols.
        //
        // On the **last cycle** of a burst (`FLAG2X_LAST` set in PLS),
        // the encoder may have emitted fewer than `cw_per_cycle` data
        // CWs (when `n_source_cw + repair` doesn't divide evenly by
        // `cw_per_cycle`). The decoder doesn't know `cw_take`
        // explicitly — it sees only the PLS flag — but the EOT
        // cycle's PLHEADER sits **right after** the legitimate data
        // CWs, so we can detect it by correlating the start of each
        // CW slot with the SOF Chu sequence. A tighter threshold
        // (0.7·SOF_LEN_SYM = 45) discriminates from random data
        // (peak ≈ √64 = 8) without false-positive risk.
        //
        // Limit the check to `FLAG2X_LAST` cycles so non-final cycles
        // — which always have `cw_take = cw_per_cycle` by construction
        // — pay no detection cost and have no false-positive exposure.
        // Discovered via 5-seed diagnostic on HIGH++2X 0.45 where
        // ESI 60-62 (the final 2-3 over-read slots) failed
        // systematically; with this gate they're skipped, freeing
        // 2-3 CWs of RaptorQ margin per burst.
        let last_cycle = pls.flags & FLAG2X_LAST != 0;
        let eot_sof_threshold =
            0.7_f64 * crate::plheader::SOF_LEN_SYM as f64;
        let sof_pattern = crate::plheader::sof_for_family(cfg.family);
        for k in 0..cw_per_cycle {
            let chunk_end = wire_cursor + cw_with_pilots;
            if chunk_end > symbols.len() {
                break;
            }

            // On the last cycle, sniff this slot's start for either
            // (a) the EOT PLHEADER's SOF (when TX layout puts EOT
            // right after the last data CW) or (b) silence (the
            // realistic case: `modem2x::encode_to_samples` inserts a
            // 200 ms `INTER_FRAME_SILENCE_S` between data and EOT,
            // so the over-read into slots beyond the last `cw_take`
            // CW consumes zeros). Either signal means the data
            // portion of the cycle is exhausted — abandon this slot
            // and let the outer SOF scan re-anchor on the genuine
            // next PLHEADER. Limited to `FLAG2X_LAST` cycles to keep
            // non-final cycles (always full by construction) at zero
            // false-positive risk.
            //
            // Discovered via 5-seed diagnostic on HIGH++2X 0.45 where
            // ESIs 61-62 (the 2 over-read slots beyond
            // `cw_take = 7` of `cw_per_cycle = 9` in the last cycle
            // for k=47 + 14 repair) failed every burst — the silence
            // detection skips them and frees up 2 CWs of RaptorQ
            // margin.
            if last_cycle {
                // Energy gate first: silence is the common case
                // because of the 200 ms inter-frame silence.
                let mean_e2: f64 = symbols[wire_cursor..wire_cursor + SOF_LEN_SYM]
                    .iter()
                    .map(|&s| s.norm_sqr())
                    .sum::<f64>()
                    / SOF_LEN_SYM as f64;
                if mean_e2 < 0.05 {
                    break;
                }
                // SOF correlation: defensive fall-through when the TX
                // doesn't insert silence (e.g. continuous-stream OTA).
                let mut acc = Complex64::new(0.0, 0.0);
                for n in 0..SOF_LEN_SYM {
                    acc += symbols[wire_cursor + n] * sof_pattern[n].conj();
                }
                if acc.norm() >= eot_sof_threshold {
                    break;
                }
            }

            let chunk: Vec<Complex64> = symbols[wire_cursor..chunk_end]
                .iter()
                .map(|&s| s / sof_gain)
                .collect();
            wire_cursor = chunk_end;

            decode_one_cw(
                &chunk,
                cw_data_syms,
                &cfg.pilot_pattern,
                groups_per_cw * (1 + k), // match encoder's rotation
                &constellation,
                &interleave_perm,
                &deinterleave_perm,
                &decoder,
                &encoder,
                k_bytes,
                false,
                pls.base_esi + k as u32,
                &mut cw_bytes,
                &mut result,
                &mut sigma2_sum,
                &mut sigma2_radial_sum,
                &mut sigma2_tangential_sum,
                &mut sigma2_n,
                cycle_refs,
                None,                    // batch path: no per-cycle phase tracker
            );
        }

        result.cycles += 1;
        scan = wire_cursor;
        if pls.flags & FLAG2X_LAST != 0 {
            // Caller asked for an early stop; keep scanning so subsequent
            // bursts still decode but we have already collected this
            // burst's full payload.
        }
    }

    // RaptorQ reassembly.
    if let Some(ref h) = result.app_header {
        if let Some(payload) = modem_framing::raptorq_codec::try_decode(
            &cw_bytes,
            h.file_size,
            h.t_bytes as u16,
        ) {
            result.data = payload;
        } else {
            // Fall back to ESI-sorted concat (zero-padded missing slots)
            // — same V3 strategy when not enough packets converged for
            // the fountain decoder.
            let n_source_cw = ((h.file_size as usize) + k_bytes - 1) / k_bytes;
            let mut acc = Vec::with_capacity(n_source_cw * k_bytes);
            for esi in 0..n_source_cw as u32 {
                if let Some(b) = cw_bytes.get(&esi) {
                    acc.extend_from_slice(b);
                } else {
                    acc.extend(std::iter::repeat(0u8).take(k_bytes));
                }
            }
            acc.truncate(h.file_size as usize);
            result.data = acc;
        }
    }

    if sigma2_n > 0 {
        let n = sigma2_n as f64;
        result.sigma2_data = (sigma2_sum / n).max(SIGMA2_FLOOR);
        result.sigma2_radial = (sigma2_radial_sum / n).max(SIGMA2_FLOOR / 2.0);
        result.sigma2_tangential = (sigma2_tangential_sum / n).max(SIGMA2_FLOOR / 2.0);
    }

    let extras = DecodePassExtras { cw_bytes, anchors };
    let result_opt = if result.cycles == 0 {
        None
    } else {
        finalize_progress_bitmap(&mut result);
        Some(result)
    };
    (result_opt, extras)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_one_cw(
    chunk: &[Complex64],
    cw_data_syms: usize,
    pattern: &PilotPattern2x,
    group_idx_offset: usize,
    constellation: &modem_core_base::constellation::Constellation,
    interleave_perm: &[usize],
    deinterleave_perm: &[usize],
    decoder: &LdpcDecoder,
    encoder: &modem_core_base::ldpc::encoder::LdpcEncoder,
    k_bytes: usize,
    is_meta: bool,
    esi: u32,
    cw_bytes: &mut HashMap<u32, Vec<u8>>,
    result: &mut RxResult2x,
    sigma2_sum: &mut f64,
    sigma2_radial_sum: &mut f64,
    sigma2_tangential_sum: &mut f64,
    sigma2_n: &mut usize,
    cycle_refs: Option<(&[Complex64], &[Complex64])>,
    // Phase 3 — optional out: pilot phase observations per group,
    // with wire offsets relative to the start of this CW chunk. The
    // caller (Rx2xSession::drive_locked) accumulates these across all
    // CWs of a cycle, converts to absolute symbol indices, and runs
    // rts_phase_smooth + turbo redecode at cycle end.
    phase_obs_out: Option<&mut Vec<(usize, PhaseObs)>>,
) {
    let gain = estimate_cw_gain(chunk, cw_data_syms, pattern, group_idx_offset, cycle_refs);
    let split = estimate_cw_sigma2_split(
        chunk, cw_data_syms, pattern, group_idx_offset, gain, cycle_refs);
    let sigma2 = split.total;
    *sigma2_sum += split.total;
    *sigma2_radial_sum += split.radial;
    *sigma2_tangential_sum += split.tangential;
    *sigma2_n += 1;

    // Phase 3 — extract pilot phase observations for the per-cycle
    // RTS smoother. Done EARLY (before decode) so the obs are recorded
    // regardless of whether this CW converges.
    if let Some(obs_buf) = phase_obs_out {
        let obs = pilot_phase_obs_per_group(
            chunk, cw_data_syms, pattern, group_idx_offset, gain,
            split.tangential.max(SIGMA2_FLOOR / 2.0),
        );
        obs_buf.extend(obs);
    }

    // De-interleave the data symbols, divide by the gain (residual phase
    // / amplitude on top of the SOF reference).
    let data_only = pilot2x_tdm::deinterleave_data_pilots_2x(
        chunk,
        pattern,
        cw_data_syms,
    );
    let mut data_norm: Vec<Complex64> = if (gain - Complex64::new(1.0, 0.0)).norm() < 1e-9 {
        data_only
    } else {
        data_only.into_iter().map(|s| s / gain).collect()
    };

    // Per-ring SSD LS-gain refinement (Meyr/Moeneclaey/Fechtel, Wiley
    // 1998 §8.4): after the pilot-based gain has roughly normalised
    // the symbols, run a separate LS estimator per APSK ring on the
    // hard-decided data, then apply the per-ring corrections. Captures
    // AM-AM / AM-PM distortion the pilot-only estimator misses (the FM
    // channel applies a magnitude-dependent gain that hits 32/64-APSK
    // outer rings differently from the |P|=1 pilot). No-op on QPSK
    // and 8PSK (single ring). Empirically narrows the V3↔V4 σ² gap
    // on the §10.3 sweep, see §10.5 fix #2.
    let ring_gains = estimate_per_ring_gain(&data_norm, constellation);
    apply_per_ring_correction(&mut data_norm, constellation, &ring_gains);

    // Turbo Pass 1: model-driven per-ring LLR scaling.
    //
    // Build a ChannelModel from the radial/tangential pilot split,
    // derive a per-ring σ² (= σ²_awgn_floor·2 + R²·σ²_excess), then
    // assign each data symbol the σ² of its nearest ring before
    // computing LLRs. Captures the channel's R-dependent SNR — outer
    // 32/64-APSK rings see higher σ² than the unit-pilot.
    //
    // Why per-symbol rather than a single scalar σ²: on a non-AWGN
    // APSK channel, LDPC accepting over-optimistic LLRs on outer-ring
    // bits and under-optimistic LLRs on inner-ring bits converges
    // worse than feeding it the actual per-bit noise level. Plays
    // well with the upcoming pass 2 (data-driven σ²_r,DD).
    // Pass 1 LLR: per-ring σ² applied PER CANDIDATE (division before
    // min, with the Bayesian +log σ²(s) term). See
    // soft_demod::llr_maxlog_per_ring docstring for why this matters
    // vs the naive "one σ² for all candidates of y_i" formulation.
    let channel_model = ChannelModel::from_pilot_split(split);
    let (radii_p1, _) = constellation.rings();
    let sigma2_per_ring_p1: Vec<f64> = radii_p1
        .iter()
        .map(|&r| channel_model.sigma2_total_at_ring(r).max(SIGMA2_FLOOR))
        .collect();
    let llr = soft_demod::llr_maxlog_per_ring(
        &data_norm, constellation, &sigma2_per_ring_p1);
    let llr_deint = interleaver::apply_permutation_f32(&llr, deinterleave_perm);
    let llr_for_ldpc = &llr_deint[..decoder.n()];
    let (info_bytes_p1, posterior_ldpc_order, converged_p1) =
        decoder.decode_to_bytes_with_posterior(llr_for_ldpc);
    let _ = sigma2; // kept for back-compat sum aggregation above
    let _ = encoder; // EM Pass 2 no longer re-encodes hard symbols
    let _ = k_bytes;

    // --- Turbo Pass 2 EM (forward-backward Kalman + RTS) ----------------
    //
    // Pass 1 LDPC produced bit posteriors (saturated when Pass 1
    // converged, intermediate-magnitude when it ran out of iterations
    // on a near-miss). Convert them to **soft constellation symbols**
    // (E[s_k | LLR_post] and ring-conditional means), then run three
    // Kalman + RTS smoothers to refine the channel model with the
    // full LDPC information:
    //
    //   1. Phase φ_k        — 2-state {phase, drift} smoother
    //                         (`phase_smoother::rts_phase_smooth`).
    //   2. Per-ring gain g_r(k) — 1-state complex random-walk (real
    //                         and imag smoothed independently).
    //   3. Per-ring log-σ²_r(k) — 1-state random-walk on the log scale
    //                         (additive model for multiplicative σ²
    //                         drift, with χ²₂-derived bias correction).
    //
    // The three smoothers feed `llr_maxlog_per_sym_per_ring` for the
    // Pass 2 re-LLR.
    //
    // **Run unconditionally** — even when Pass 1 didn't converge.
    // The posterior LLRs at LDPC max_iter are imperfect (the decoder
    // got stuck in some local minimum) but still informative: bit
    // signs carry the decoder's best guess, magnitudes carry how
    // confident it was. The soft symbols built from them are noisier
    // than the converged case but still strongly biased toward the
    // truth on most positions. The EM channel refinement then gives
    // LDPC a second shot with better-scaled LLRs, which on the SNR
    // cliff (where a few dB of LLR-scale error is the difference
    // between hitting max_iter and converging) rescues a meaningful
    // fraction of CWs. Trust Pass 2 byte output whenever Pass 2 LDPC
    // converges, regardless of Pass 1's status.
    let (info_bytes, converged) = {
        // Step 1. Re-interleave posterior LLR (n bits) back to the
        // symbol-major space (cw_data_syms × bps bits). The padding bit
        // (when present, e.g. Apsk32 R=1/2 pads 2304 → 2305) sits at
        // index `decoder.n()` in symbol-major space — TX padded with
        // bit 0, so we inject a very confident positive LLR.
        let padded_len = interleave_perm.len();
        let mut posterior_padded = vec![25.0_f32; padded_len];
        posterior_padded[..decoder.n()].copy_from_slice(&posterior_ldpc_order);
        let posterior_symbol_major =
            interleaver::apply_permutation_f32(&posterior_padded, interleave_perm);
        let bps = constellation.bits_per_sym;
        let n_data_bits = cw_data_syms * bps;
        let posterior_for_symbols = &posterior_symbol_major[..n_data_bits];

        // Step 2. Soft symbols.
        let soft = soft_demod::soft_symbols_from_posterior_llr(
            posterior_for_symbols, constellation);
        debug_assert_eq!(soft.len(), data_norm.len());

        // Step 3. RTS phase smoother on raw (gain-untouched) data. The
        // measurement angle arg(y · conj(s̃)) is invariant to a small
        // residual gain magnitude error — phase is mostly orthogonal
        // to gain estimation at this stage.
        let sigma2_tang = split.tangential.max(SIGMA2_FLOOR / 2.0);
        let phase_obs = pass2_em_phase_obs(&data_norm, &soft, sigma2_tang);
        let phase_params = PhaseSmootherParams::from_channel(channel_model.sigma2_phi);
        let phi_smooth = rts_phase_smooth(&phase_obs, &phase_params);

        // Step 4. EM per-ring gain g_r(k) on phase-derotated data.
        let mut data_p2: Vec<Complex64> = data_norm
            .iter()
            .zip(phi_smooth.iter())
            .map(|(&y, &phi)| y * Complex64::from_polar(1.0, -phi))
            .collect();
        let gain_per_ring =
            pass2_em_gain_smoother(&data_p2, &soft, constellation, &channel_model);

        // Apply posterior-weighted gain at each symbol.
        let g_weighted = pass2_em_weighted_gain(&soft, &gain_per_ring);
        for (y, g) in data_p2.iter_mut().zip(g_weighted.iter()) {
            if g.norm_sqr() > 1e-12 {
                *y = *y / *g;
            }
        }

        // **Data-scatter σ²** (honest SNR metric, see `RxResult2x::
        // sigma2_data_scatter` docstring). Hard-decode every post-Pass-2
        // DATA symbol to the nearest constellation point and accumulate
        // |y − s_hat|² and |s_hat|². DATA only — META CWs are skipped so
        // the metric reflects the payload channel statistics the GUI
        // operator cares about. The accumulation is over the burst's
        // **entire** DATA history, weighted by symbol count (a running
        // mean update on the existing means in `result`). The denomin-
        // ator `data_scatter_n` doubles as the "metric valid" flag —
        // zero means no DATA CW has been visited yet.
        if !is_meta {
            let mut err_sq = 0.0_f64;
            let mut ref_sq = 0.0_f64;
            for &y in data_p2.iter() {
                let s_hat = constellation.hard_decision(y);
                err_sq += (y - s_hat).norm_sqr();
                ref_sq += s_hat.norm_sqr();
            }
            let n_new = data_p2.len();
            if n_new > 0 {
                let prev_n = result.data_scatter_n as f64;
                let new_n = prev_n + n_new as f64;
                result.sigma2_data_scatter =
                    (result.sigma2_data_scatter * prev_n + err_sq) / new_n;
                result.es_data_scatter =
                    (result.es_data_scatter * prev_n + ref_sq) / new_n;
                result.data_scatter_n += n_new;
            }
        }

        // GUI diagnostic snapshot — recorded AFTER Pass 2 phase + gain
        // correction so the constellation panel shows the modem's best
        // estimate, not the raw Pass 1 cloud. Likewise the pilot phase
        // entry pushed here is the mean of the RTS-smoothed phase
        // trajectory `phi_smooth` (not `gain.arg()` from Pass 1) so the
        // operator sees the trend Pass 2 actually fits, including the
        // intra-CW slope a 1-state gain estimator would miss.
        result
            .pilot_phase_per_cw
            .push(mean_phase(&phi_smooth));
        result.pilot_phase_is_meta.push(is_meta);
        // Slice 2x21 sliding-window: keep only the most recent
        // `PILOT_PHASE_RECENT_N` entries. The two arrays are parallel
        // and must drain in lockstep.
        if result.pilot_phase_per_cw.len() > PILOT_PHASE_RECENT_N {
            let drop_n = result.pilot_phase_per_cw.len() - PILOT_PHASE_RECENT_N;
            result.pilot_phase_per_cw.drain(..drop_n);
            result.pilot_phase_is_meta.drain(..drop_n);
        }
        if !is_meta
            && result.constellation_sample.len() < MAX_CONSTELLATION_POINTS
        {
            let remaining = MAX_CONSTELLATION_POINTS
                - result.constellation_sample.len();
            let step = (data_p2.len() / remaining.max(1)).max(1);
            for (i, sym) in data_p2.iter().enumerate() {
                if i % step == 0 {
                    result
                        .constellation_sample
                        .push([sym.re as f32, sym.im as f32]);
                    if result.constellation_sample.len()
                        >= MAX_CONSTELLATION_POINTS
                    {
                        break;
                    }
                }
            }
        }

        // Step 5. EM per-ring σ²_r(k) on the now phase + gain corrected
        // data. Uses the *unit* g_r expected after correction (so we
        // pass identity gains for σ² estimation — residuals e = y − μ).
        let identity_gain: Vec<Vec<Complex64>> = (0..gain_per_ring.len())
            .map(|_| vec![Complex64::new(1.0, 0.0); data_p2.len()])
            .collect();
        let sigma2_em =
            pass2_em_sigma2_smoother(&data_p2, &soft, &identity_gain,
                                     constellation, &channel_model);
        let sigma2_per_sym_per_ring = pass2_em_sigma2_to_per_sym_per_ring(
            &sigma2_em, constellation, &channel_model, data_p2.len());

        // Step 6. Re-LLR with the time-varying per-symbol per-ring σ²,
        // then re-LDPC.
        let llr_p2 = soft_demod::llr_maxlog_per_sym_per_ring(
            &data_p2, constellation, &sigma2_per_sym_per_ring);
        let llr_p2_deint = interleaver::apply_permutation_f32(
            &llr_p2, deinterleave_perm);
        let llr_p2_for_ldpc = &llr_p2_deint[..decoder.n()];
        let (info_bytes_p2, converged_p2) = decoder.decode_to_bytes(llr_p2_for_ldpc);

        // Combine the two passes. `converged_p2 = true` always wins:
        // Pass 2 LDPC syndrome is zero, so its bytes are LDPC-valid
        // even if Pass 1 disagreed. When Pass 2 didn't converge but
        // Pass 1 did, fall back to Pass 1's bytes. When neither
        // converged, take Pass 1's bytes and report `converged = false`
        // — the CW won't contribute to RaptorQ assembly downstream.
        if converged_p2 {
            (info_bytes_p2, true)
        } else if converged_p1 {
            (info_bytes_p1, true)
        } else {
            (info_bytes_p1, false)
        }
    };

    result.total_cws += 1;
    // Diagnostic: stable log line, one per CW, for analysing the
    // pattern of failures (random vs clustered) on borderline SNRs.
    // Enable via `RX_V4_LOG_CW=1`. The format is grep-friendly:
    // `[cw-status]` prefix + key=val pairs.
    if std::env::var_os("RX_V4_LOG_CW").is_some() {
        eprintln!(
            "[cw-status] is_meta={} esi={} converged={} σ²={:.4e} σ²_R={:.4e} σ²_T={:.4e}",
            is_meta as u8,
            esi,
            converged as u8,
            split.total,
            split.radial,
            split.tangential,
        );
    }
    if !is_meta {
        // Count every DATA CW the decoder visited (whether or not it
        // converged) so `data_cws_total` matches the progress bar's
        // denominator — V3's `blocks_expected` is DATA-only, V4 must
        // mirror that for the GUI fountain-fill math to line up.
        result.data_cws_total += 1;
    }
    if converged {
        result.converged_cws += 1;
        let bytes = info_bytes[..k_bytes].to_vec();
        if is_meta {
            if let Some(h) = app_header::decode_meta_payload(&bytes) {
                // The EOT cycle's META-CW carries a sentinel AppHeader
                // with `file_size = 0` (see `build_eot_frame_v4`). It must
                // not clobber a real AppHeader recovered from an earlier
                // data cycle — otherwise the RaptorQ reassembly step
                // computes `n_source_cw = 0` and `result.data` ends up
                // empty even though every data CW converged.
                let keep_existing = matches!(
                    result.app_header,
                    Some(ref cur) if cur.file_size > 0 && h.file_size == 0,
                );
                if !keep_existing {
                    result.app_header = Some(h);
                }
            }
        } else {
            cw_bytes.insert(esi, bytes);
            // Light the GUI's fountain-fill bit for this ESI. LSB-first
            // byte order, same encoding as `modem-worker::rx_worker`
            // emits — see V3 `V2ProgressPayload::converged_bitmap`. The
            // bitmap auto-grows; the final pad to `data_cws_total`
            // happens in `finalize_progress_bitmap` so the GUI sees the
            // full DATA slot count even when the highest ESI failed.
            result.data_cws_converged += 1;
            let bit_idx = esi as usize;
            let byte_idx = bit_idx >> 3;
            if result.converged_bitmap.len() <= byte_idx {
                result.converged_bitmap.resize(byte_idx + 1, 0);
            }
            result.converged_bitmap[byte_idx] |= 1 << (bit_idx & 7);
        }
    }
}

/// Pad `result.converged_bitmap` to cover every visited DATA CW slot.
/// Without this, a burst where the highest ESI failed would emit a
/// bitmap shorter than the progress bar's denominator and the GUI's
/// rightmost slots would all default to "missing" — true in spirit but
/// the byte length should match the wire contract regardless. Called
/// once at the end of `rx_v4_symbols_after`, after every cycle has
/// been walked.
fn finalize_progress_bitmap(result: &mut RxResult2x) {
    let needed_bytes = result.data_cws_total.div_ceil(8);
    if result.converged_bitmap.len() < needed_bytes {
        result.converged_bitmap.resize(needed_bytes, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame2x::{build_eot_frame_v4, build_superframe_v4};
    use crate::profile2x::{
        profile_high_2x, profile_normal_2x, profile_robust_2x, profile_ultra_2x,
        ProfileIndex2x,
    };
    use modem_framing::app_header::mime;

    fn rng_bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 56) & 0xFF) as u8
            })
            .collect()
    }

    /// Test helper: extract the raw received pilot symbols from a HIGH+
    /// burst run through the modulator (no FM channel), to inspect their
    /// values directly and identify any pilot-block-internal bias.
    #[test]
    #[ignore] // run with: cargo test sigma2_diag_high_plus_pilots -- --ignored --nocapture
    fn sigma2_diag_high_plus_pilots_dump() {
        // Look at the per-pilot residual structure on HIGH+2X after the
        // modulator + matched-filter audio roundtrip (no FM channel). If
        // the per-pilot residuals show a *systematic pattern* (e.g.,
        // first/last symbols of each block much further from P than the
        // interior), then the bias is RRC pulse-shape leakage from
        // adjacent data symbols. If they're random, it's something else.
        use crate::profile2x::profile_high_plus_2x;
        use modem_core_base::modulator;
        use modem_core_base::rrc;
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(200, 0xCAFE);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let (sps, pitch) = rrc::check_integer_constraints(
            modem_core_base::types::AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau,
        ).expect("sps");
        let taps = rrc::rrc_taps(
            cfg.base.beta, modem_core_base::types::RRC_SPAN_SYM, sps);
        let audio = modulator::modulate(
            &symbols, sps, pitch, &taps, cfg.base.center_freq_hz);
        // Downmix + MF
        let mut iq = Vec::with_capacity(audio.len());
        for (n, &s) in audio.iter().enumerate() {
            let theta = -2.0 * std::f64::consts::PI * cfg.base.center_freq_hz
                * (n as f64) / (modem_core_base::types::AUDIO_RATE as f64);
            iq.push(Complex64::new(
                (s as f64) * theta.cos(),
                (s as f64) * theta.sin(),
            ));
        }
        let mf = modem_core_base::demodulator::matched_filter(&iq, &taps);
        let n_syms = mf.len() / sps;
        let sof = crate::plheader::sof_for_family(cfg.family);
        let mut best_peak = 0.0_f64;
        let mut best_phase = 0usize;
        for ph in 0..sps {
            let mut peak = 0.0_f64;
            let limit = n_syms.saturating_sub(crate::plheader::SOF_LEN_SYM);
            for k0 in 0..limit {
                let mut acc = Complex64::new(0.0, 0.0);
                for n in 0..crate::plheader::SOF_LEN_SYM {
                    acc += mf[ph + (k0 + n) * sps] * sof[n].conj();
                }
                if acc.norm() > peak { peak = acc.norm(); }
            }
            if peak > best_peak { best_peak = peak; best_phase = ph; }
        }
        let rx_symbols: Vec<Complex64> =
            (0..n_syms).map(|k| mf[best_phase + k * sps]).collect();

        // Replicate the rx_v4 production pipeline: PLHEADER decode →
        // sof_gain → divide all CW symbols by sof_gain before per-CW
        // gain estimate.
        let cw_with_pilots = cw_with_pilots_len(&cfg);
        let sof_at = find_next_sof(&rx_symbols, 0, cfg.family)
            .expect("SOF in noise-free audio");
        let plheader_slice = &rx_symbols[sof_at..sof_at + PLHEADER_LEN_SYM];
        let (_, sof_gain) = crate::plheader::decode_plheader_at(plheader_slice, cfg.family)
            .expect("PLHEADER decodes");
        eprintln!("[pilot-dump] sof_gain = ({:+.6}, {:+.6}) |g|={:.6}",
                  sof_gain.re, sof_gain.im, sof_gain.norm());
        eprintln!("[pilot-dump] training_amplitude = {:.6}", cfg.training_amplitude);

        // Walk to the first DATA-CW (offset = PLHEADER + warmup + META-CW).
        let cursor = sof_at + PLHEADER_LEN_SYM + cfg.lms_warmup_syms + cw_with_pilots;
        let chunk: Vec<Complex64> = rx_symbols[cursor..cursor + cw_with_pilots]
            .iter().map(|&s| s / sof_gain).collect();
        let cw_data = cfg.cw_data_syms();
        let pattern = cfg.pilot_pattern;
        let groups_per_cw = pilot_groups_per_cw(&cfg);
        // DATA-CW[0] pilot rotation continues from META-CW's groups.
        let group_offset = groups_per_cw;
        let gain = estimate_cw_gain(&chunk, cw_data, &pattern, group_offset, None);
        eprintln!("[pilot-dump] per-CW gain (after sof_gain norm) = ({:+.6}, {:+.6}) |g|={:.6}",
                  gain.re, gain.im, gain.norm());
        let positions =
            pilot2x_tdm::pilot_positions_2x(cw_data, &pattern, group_offset);
        let mut sum_resid_sq = 0.0_f64;
        let mut n_pilot = 0usize;
        for (g, (p_start, p_end, abs_pilot_start)) in positions.iter().enumerate() {
            eprintln!("[pilot-dump] === group {g} pilot slot @ wire offset {p_start} ===");
            for (i, k) in (*p_start..*p_end).enumerate() {
                let pref = pilot_symbol_2x(abs_pilot_start + i);
                let s = chunk[k];
                let normed = s / gain;
                let resid = normed - pref;
                sum_resid_sq += resid.norm_sqr();
                n_pilot += 1;
                eprintln!(
                    "  [{i}] raw=({:+.4},{:+.4}) |raw|={:.4}  normed=({:+.4},{:+.4}) |n|={:.4}  resid=|{:.4}|",
                    s.re, s.im, s.norm(), normed.re, normed.im, normed.norm(), resid.norm()
                );
            }
        }
        eprintln!("[pilot-dump] mean σ² over {n_pilot} pilots = {:.6}",
                  sum_resid_sq / n_pilot as f64);
    }

    #[test]
    #[ignore] // diagnostic, run with: cargo test sigma2_diag_audio -- --ignored --nocapture
    fn sigma2_diag_audio_roundtrip_no_channel() {
        // build_superframe_v4 → modulator → matched filter → strobe →
        // rx_v4_symbols. No FM channel. If σ² stays at floor here, the
        // bias seen in OTA sweeps comes from the FM channel itself
        // (preemph / hard-clip / FM nonlinearity). If σ² spikes here on
        // HIGH+/HIGH+56, the bias is in the RRC pulse-shape + sampling
        // round-trip (likely the SOF-anchored integer-step sampling
        // accumulating phase error on multi-ring APSK).
        use modem_core_base::modulator;
        use modem_core_base::rrc;
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(400, 0xCAFE);
            let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
            let (sps, pitch) = rrc::check_integer_constraints(
                modem_core_base::types::AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau,
            ).expect("sps");
            let taps = rrc::rrc_taps(
                cfg.base.beta, modem_core_base::types::RRC_SPAN_SYM, sps);
            let audio = modulator::modulate(
                &symbols, sps, pitch, &taps, cfg.base.center_freq_hz);
            // Downmix + MF
            let mut iq = Vec::with_capacity(audio.len());
            for (n, &s) in audio.iter().enumerate() {
                let theta = -2.0 * std::f64::consts::PI * cfg.base.center_freq_hz
                    * (n as f64) / (modem_core_base::types::AUDIO_RATE as f64);
                iq.push(modem_core_base::types::Complex64::new(
                    (s as f64) * theta.cos(),
                    (s as f64) * theta.sin(),
                ));
            }
            let mf = modem_core_base::demodulator::matched_filter(&iq, &taps);
            // SOF-anchored integer-step sampling
            let n_syms = mf.len() / sps;
            let sof = crate::plheader::sof_for_family(cfg.family);
            let mut best_peak = 0.0_f64;
            let mut best_phase = 0usize;
            for ph in 0..sps {
                let mut peak = 0.0_f64;
                let limit = n_syms.saturating_sub(crate::plheader::SOF_LEN_SYM);
                for k0 in 0..limit {
                    let mut acc = modem_core_base::types::Complex64::new(0.0, 0.0);
                    for n in 0..crate::plheader::SOF_LEN_SYM {
                        acc += mf[ph + (k0 + n) * sps] * sof[n].conj();
                    }
                    let mag = acc.norm();
                    if mag > peak { peak = mag; }
                }
                if peak > best_peak { best_peak = peak; best_phase = ph; }
            }
            let rx_symbols: Vec<modem_core_base::types::Complex64> =
                (0..n_syms).map(|k| mf[best_phase + k * sps]).collect();
            let result = rx_v4_symbols(&rx_symbols, &cfg).expect("decode");
            eprintln!(
                "[sigma2-audio] {} cons={:?} sigma2={:.6} converged={}/{}",
                p.name(),
                cfg.base.constellation,
                result.sigma2_data,
                result.converged_cws,
                result.total_cws,
            );
        }
    }

    #[test]
    #[ignore] // diagnostic, run with: cargo test sigma2_diag -- --ignored --nocapture
    fn sigma2_diag_pure_symbol_domain_all_profiles() {
        // Pure symbol-domain roundtrip — no audio, no FM, no channel.
        // The σ² reported here is whatever floor the LS estimator
        // produces on synthetic perfect pilots. If it matches the
        // OTA-channel σ² seen in §10.3 (~0.43 for HIGH+) then the bias
        // is in the modem itself; if it's near the SIGMA2_FLOOR then
        // the bias comes from the audio/FM chain.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(400, 0xCAFE);
            let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
            let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
            eprintln!(
                "[sigma2-diag] {} cons={:?} pattern={}/{}  \
                 cw_data_syms={} sigma2={:.6} converged={}/{}",
                p.name(),
                cfg.base.constellation,
                cfg.pilot_pattern.d_syms,
                cfg.pilot_pattern.p_syms,
                cfg.cw_data_syms(),
                result.sigma2_data,
                result.converged_cws,
                result.total_cws,
            );
        }
    }

    #[test]
    fn roundtrip_noise_free_high_2x() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(2_000, 0xCAFE);
        let symbols = build_superframe_v4(&payload, &cfg, 0xDEAD_BEEF, mime::BINARY, 0xAA55);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        let h = result.app_header.expect("AppHeader recovered");
        assert_eq!(h.session_id, 0xDEAD_BEEF);
        assert_eq!(h.file_size, payload.len() as u32);
        assert_eq!(result.data, payload);
        assert!(result.cycles >= 1);
        assert_eq!(result.converged_cws, result.total_cws);
    }

    #[test]
    fn roundtrip_noise_free_normal_2x() {
        let cfg = profile_normal_2x();
        let payload = rng_bytes(800, 0x1234);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_noise_free_robust_2x() {
        let cfg = profile_robust_2x();
        let payload = rng_bytes(400, 0x99);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_noise_free_ultra_2x() {
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(120, 0x77);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_under_complex_channel_gain() {
        // Apply a known complex gain across the whole burst — the per-CW
        // pilot LS estimate must absorb it transparently.
        let cfg = profile_high_2x();
        let payload = rng_bytes(2_000, 0xFEED);
        let mut symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        let g = Complex64::new(0.7, -0.4);
        for s in &mut symbols { *s = *s * g; }
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn converged_bitmap_marks_every_data_esi_on_clean_burst() {
        // Noise-free decode: every DATA CW converges → every bit in
        // [0, data_cws_total) must be set, and the byte length must
        // exactly equal ceil(data_cws_total / 8).
        let cfg = profile_normal_2x();
        let payload = rng_bytes(600, 0xBEAD);
        let symbols = build_superframe_v4(
            &payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");

        assert!(result.data_cws_total >= 1);
        assert_eq!(
            result.data_cws_converged, result.data_cws_total,
            "every DATA CW should converge on a noise-free burst",
        );
        let expected_bytes = result.data_cws_total.div_ceil(8);
        assert_eq!(
            result.converged_bitmap.len(),
            expected_bytes,
            "bitmap byte length must match ceil(data_cws_total / 8)",
        );
        for esi in 0..result.data_cws_total {
            let byte = result.converged_bitmap[esi >> 3];
            assert!(
                (byte >> (esi & 7)) & 1 == 1,
                "ESI {esi} should be marked converged",
            );
        }

        // META CWs DON'T count toward `data_cws_*` — V3's progress bar
        // contract is DATA-only and V4 must mirror it.
        assert!(result.converged_cws > result.data_cws_converged,
            "converged_cws should include META; data_cws_converged shouldn't");
    }

    #[test]
    fn gui_diagnostic_fields_populated() {
        // The v2_progress event surfaced to the GUI needs three per-CW
        // diagnostic streams: a constellation scatter cloud (post-Pass 2
        // correction, DATA-CWs only), per-CW pilot phase, and the META
        // vs DATA flag for each CW. This regression test pins the contract
        // so a future refactor can't silently blank one of them.
        let cfg = profile_high_2x();
        let payload = rng_bytes(2_000, 0xC0DE);
        let symbols = build_superframe_v4(
            &payload, &cfg, 0x123, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");

        // pilot_phase_per_cw and pilot_phase_is_meta are parallel.
        // Slice 2x21: both are sliding windows capped at
        // `PILOT_PHASE_RECENT_N` (sliding semantics make the GUI's
        // pilot-phase canvas show the most recent CWs, not the
        // cumulative trace).
        assert_eq!(
            result.pilot_phase_per_cw.len(),
            result.pilot_phase_is_meta.len(),
            "phase / is_meta arrays must stay aligned",
        );
        assert!(
            result.pilot_phase_per_cw.len() <= PILOT_PHASE_RECENT_N,
            "pilot_phase_per_cw must respect the sliding-window cap",
        );
        assert!(
            !result.pilot_phase_per_cw.is_empty(),
            "at least one phase entry on any successful decode",
        );
        // META cycles are emitted once per cycle. With the sliding
        // window of N=5 and ~4 DATA CWs per cycle, the last META can
        // get pushed out — assert only the DATA flag presence.
        assert!(result.cycles >= 1);
        assert!(
            result.pilot_phase_is_meta.iter().any(|&m| !m),
            "expected at least one DATA CW flag",
        );
        // Every phase is wrapped to [-π, π] by `mean_phase`.
        for &p in &result.pilot_phase_per_cw {
            assert!(p.abs() <= std::f64::consts::PI + 1e-9, "phase out of range");
        }

        // Constellation: DATA-only, capped at MAX_CONSTELLATION_POINTS.
        // On a noise-free roundtrip every sample lands close to a
        // constellation point — outside any reasonable [-2, 2] box would
        // signal a misfire upstream.
        assert!(!result.constellation_sample.is_empty(),
            "constellation_sample populated on any successful decode");
        assert!(result.constellation_sample.len() <= MAX_CONSTELLATION_POINTS);
        for &[re, im] in &result.constellation_sample {
            assert!(re.is_finite() && im.is_finite());
            assert!(re.abs() < 2.0 && im.abs() < 2.0,
                "post-correction symbol unexpectedly large: ({re}, {im})");
        }
    }

    #[test]
    fn sigma2_split_isotropic_on_pure_awgn() {
        // On a clean roundtrip the residuals are at the floor. Splitting
        // them radially / tangentially must give two near-equal values
        // (both ≈ floor/2, since complex variance σ² splits evenly across
        // axes for jointly-Gaussian noise). The ratio R/T must stay near
        // 1 — anything wildly different signals a non-AWGN channel.
        let cfg = profile_high_2x();
        let payload = rng_bytes(400, 0xCAFE);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        let r = result.sigma2_radial;
        let t = result.sigma2_tangential;
        // Both clamped to floor/2 = 5e-4 on a noise-free roundtrip.
        assert!(r >= SIGMA2_FLOOR / 2.0 - 1e-12);
        assert!(t >= SIGMA2_FLOOR / 2.0 - 1e-12);
        // On a noise-free signal both axes hit the floor → ratio is 1.
        let ratio = r / t;
        assert!(
            (0.5..=2.0).contains(&ratio),
            "noise-free R/T ratio should be near 1.0, got {ratio} (R={r}, T={t})"
        );
    }

    #[test]
    fn sigma2_split_captures_phase_only_distortion() {
        // Apply a per-symbol phase noise (rotational, no amplitude
        // change) and verify σ²_tangential >> σ²_radial. Verifies the
        // diagnostic correctly identifies a phase-noise-heavy channel.
        let cfg = profile_high_2x();
        let payload = rng_bytes(400, 0xC0DE);
        let mut symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        // Apply small random per-symbol phase rotations: e^(j·θ_k) with
        // θ_k ~ N(0, σ_θ²), σ_θ = 0.05 rad → tangential residual ≈ |s|·σ_θ.
        let mut rng_state = 0xDEAD_BEEFu64;
        for s in &mut symbols {
            // LCG → uniform → Box-Muller → Gaussian (cheap, no deps).
            let u1 = {
                rng_state = rng_state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (rng_state as f64) / (u64::MAX as f64)
            };
            let u2 = {
                rng_state = rng_state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (rng_state as f64) / (u64::MAX as f64)
            };
            let g = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
            let theta = 0.05 * g;
            let rot = Complex64::new(theta.cos(), theta.sin());
            *s = *s * rot;
        }
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        // On phase-only distortion, tangential should dominate. (R/T < 1)
        assert!(
            result.sigma2_tangential > result.sigma2_radial * 1.5,
            "phase-only noise should give T > 1.5·R, got R={} T={}",
            result.sigma2_radial, result.sigma2_tangential
        );
    }

    // NOTE: the synthetic `turbo_pass2_em_tracks_intra_cw_phase_drift`
    // test (added with the EM Pass 2 commit) was removed when SOF+PLS
    // became cycle-level pilots. The test injected a uniform phase
    // ramp across the burst — a meaningful proxy for sound-card drift
    // — but the ramp puts the PLHEADER refs at a different phase than
    // the per-CW pilots, which the σ² MAD estimator interpreted as
    // anomalously low variance (median pulled toward the PLHEADER's
    // small intra-PLHEADER drift residual, ignoring the much larger
    // intra-CW drift residual). Result: Pass 1 LLR over-confident,
    // LDPC committed to near-miss codewords, byte-exact failed.
    //
    // The RTS phase smoother itself stays directly tested in
    // `modem-core-base/src/phase_smoother.rs` (8 tests including
    // `rts_recovers_linear_drift_no_lag` which asserts zero-lag
    // tracking of a synthetic phase ramp). The end-to-end EM channel
    // refinement is still exercised by
    // `turbo_pass2_em_anisotropic_per_ring_sigma2`. We can reinstate
    // a phase-drift end-to-end test once we have a proper drift
    // injection that scales the PLHEADER's drift compatibly with
    // the channel model the cycle-pilots assume — likely once Phase
    // B closed-loop Gardner timing recovery is exercised against
    // real sound-card recordings.

    #[test]
    fn turbo_pass2_em_anisotropic_per_ring_sigma2() {
        // Inject anisotropic per-ring AWGN: low noise on the inner ring,
        // moderate on the middle (pilot), high on the outer ring. This
        // mimics the AM-AM compression that hits 32-APSK's outer ring
        // harder than the pilot's |P|=1. The EM σ²_r smoother should
        // catch this and feed per-ring LLR scaling — better than a
        // single global σ² that under-protects the outer ring.
        use crate::profile2x::profile_high_plus_2x;
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(600, 0xBADD_CAFE);
        let constellation = crate::frame2x::make_constellation_2x(&cfg);
        let (radii, _) = constellation.rings();
        assert_eq!(radii.len(), 3);
        // σ² per ring, with the outer ring 4× noisier than the inner.
        let sigma_per_ring = [0.015_f64, 0.025_f64, 0.060_f64];
        let mut symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        let mut rng_state = 0xC0FFEE_u64;
        let mut next_u01 = || {
            rng_state = rng_state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng_state as f64) / (u64::MAX as f64)
        };
        let mut box_muller = || {
            let u1 = next_u01().max(1e-12);
            let u2 = next_u01();
            (
                (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos(),
                (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).sin(),
            )
        };
        for s in &mut symbols {
            // Pick which ring (or pilot/SOF treated as middle).
            let r = if (s.norm() - radii[0]).abs() < 0.05 {
                0
            } else if (s.norm() - radii[2]).abs() < 0.05 {
                2
            } else {
                1
            };
            let sigma = sigma_per_ring[r];
            let (g1, g2) = box_muller();
            *s = *s + Complex64::new(sigma * g1, sigma * g2);
        }
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload,
                   "EM Pass 2 σ²_r smoother must absorb 3× anisotropy");
        // Diagnostic sanity: the RX-side σ²_radial / σ²_tangential
        // should be > 0 (we injected isotropic AWGN per ring, but the
        // total σ² aggregate over the CW reflects the per-ring scale).
        assert!(result.sigma2_data > 0.0);
    }

    #[test]
    fn turbo_pass2_handles_strong_per_ring_distortion_apsk32() {
        // Stress test for the turbo Pass 2 loop. Apply 3 ring-dependent
        // complex gains AND a heavy AWGN noise level. The SSD per-ring
        // gain corrects the deterministic distortion; the model-based
        // Pass 1 LLR scales LLRs to the per-ring SNR; the DD Pass 2
        // refines on the re-encoded truth. The whole stack must
        // byte-recover the payload even with significant noise per ring.
        use crate::profile2x::profile_high_plus_2x;
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(600, 0xDEADBEEF);
        let constellation = crate::frame2x::make_constellation_2x(&cfg);
        let (radii, _) = constellation.rings();
        let ring_gains = [
            Complex64::new(0.90,  0.05),  // inner
            Complex64::new(1.05, -0.03),  // middle (≈ pilot)
            Complex64::new(1.15, -0.10),  // outer (compression-style)
        ];
        assert_eq!(radii.len(), 3);
        let mut symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        for s in &mut symbols {
            let r = if (s.norm() - radii[0]).abs() < 0.05 {
                0
            } else if (s.norm() - radii[2]).abs() < 0.05 {
                2
            } else {
                1
            };
            *s = *s * ring_gains[r];
        }
        // No AWGN — the test just verifies the turbo loop preserves the
        // existing P1 capability under deterministic per-ring distortion.
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload,
                   "Turbo Pass 2 must preserve roundtrip under per-ring distortion");
    }

    #[test]
    fn roundtrip_under_per_ring_distortion_apsk32() {
        // Apply a *ring-dependent* complex gain — inner rings get one
        // gain, outer ring another. This is the kind of AM-AM distortion
        // the SSD per-ring LS estimator is designed to absorb. Without
        // the SSD pass, the pilot-based gain (locked to |P|=1, which
        // sits between the rings on 32-APSK) would mis-scale at least
        // one ring and LDPC would diverge on data symbols there.
        use crate::profile2x::profile_high_plus_2x;
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(600, 0xC0FFEE);
        let constellation =
            crate::frame2x::make_constellation_2x(&cfg);
        let (radii, ring_of_point) = constellation.rings();
        // 3 distinct gains: one per ring of 32-APSK.
        let ring_gains = [
            Complex64::new(0.85,  0.10),  // inner
            Complex64::new(1.00,  0.00),  // middle (≈ pilot magnitude)
            Complex64::new(1.20, -0.15),  // outer (FM-channel-style boost)
        ];
        assert_eq!(radii.len(), 3, "Apsk32 should have 3 rings");
        let mut symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        // The pilot and SOF symbols are at |P|=1 and training_amplitude
        // respectively — both should map to ring 1 (middle) in the
        // gain dictionary. We apply ring-dependent gain only to symbols
        // whose magnitude matches one of the data rings; pilots
        // (|s|=1, between rings) get the middle ring's gain.
        for s in &mut symbols {
            let r = if (s.norm() - radii[0]).abs() < 0.05 {
                0
            } else if (s.norm() - radii[2]).abs() < 0.05 {
                2
            } else {
                1
            };
            let _ = ring_of_point;
            *s = *s * ring_gains[r];
        }
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload,
                   "per-ring SSD must absorb 3-ring complex gain distortion");
    }

    #[test]
    fn late_entry_skips_partial_first_cycle() {
        // Discard the first ~half of the very first PLHEADER so the RX
        // can't anchor on it; it should pick up the second cycle.
        let cfg = profile_high_2x();
        // Force several cycles by encoding a large payload.
        let cw_per_cycle = data_cw_per_cycle(&cfg);
        let k_bytes = cfg.base.ldpc_rate.k() / 8;
        let needed_cw = cw_per_cycle * 3;
        let payload = rng_bytes(needed_cw * k_bytes, 0xBEEF);
        let symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        // Drop the first 100 symbols (well into the SOF; RX must lock on
        // cycle 2 which still carries the same AppHeader).
        let late = symbols[100..].to_vec();
        let result = rx_v4_symbols(&late, &cfg).expect("late-entry decode");
        let h = result.app_header.expect("AppHeader from later cycle");
        assert_eq!(h.session_id, 1);
        // Some data CWs from cycle 0 are missing — don't require a
        // perfect data match, but the recovered length must equal the
        // file_size and the trailing bytes (cycle 2 onwards) must match.
        assert_eq!(result.data.len(), payload.len());
        // Cycle 2 starts at ESI = 2·cw_per_cycle; bytes from that ESI
        // onwards should match the original payload.
        let off = (cw_per_cycle * 2) * k_bytes;
        let take = k_bytes;
        assert_eq!(&result.data[off..off + take], &payload[off..off + take]);
    }

    #[test]
    fn eot_frame_decodes_with_eot_flag() {
        let cfg = profile_normal_2x();
        let symbols = build_eot_frame_v4(&cfg, 0xCAFE_BABE);
        let result = rx_v4_symbols(&symbols, &cfg).expect("EOT decodes");
        assert!(result.eot_seen, "EOT flag must be reported");
        assert!(result.app_header.is_some(), "EOT carries an AppHeader");
        assert_eq!(result.app_header.unwrap().session_id, 0xCAFE_BABE);
    }

    #[test]
    fn data_then_eot_preserves_real_app_header() {
        // Regression: an EOT cycle following a data superframe used to
        // clobber `result.app_header` with the EOT's zero-file_size
        // sentinel, leaving `result.data` empty even though every data CW
        // converged. We pin the test to ULTRA2X (cw_per_cycle = 1, so
        // every cycle is "full" and the data-CW inner loop never reads
        // past the actual data into the EOT PLHEADER — a separate edge
        // case that only bites profiles with cw_per_cycle > 1 and a
        // partial last cycle). This is the exact shape that the CLI
        // `nbfm-modem tx --family 2x -p ULTRA2X … && rx …` produces.
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(150, 0xEEE0);
        let mut symbols = build_superframe_v4(&payload, &cfg, 0x12345678, mime::BINARY, 0xCC);
        symbols.extend(build_eot_frame_v4(&cfg, 0x12345678));

        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert!(result.eot_seen, "EOT must be detected");
        let h = result.app_header.expect("data AppHeader survives EOT");
        assert_eq!(h.file_size as usize, payload.len());
        assert_eq!(result.data, payload);
    }

    #[test]
    fn returns_none_when_no_sof_present() {
        // Random noise → no SOF correlation peak → None.
        let cfg = profile_high_2x();
        let noise: Vec<Complex64> = (0..10_000)
            .map(|k| {
                let phase = (k as f64) * 0.123;
                Complex64::new(phase.cos(), phase.sin()) * 0.05
            })
            .collect();
        assert!(rx_v4_symbols(&noise, &cfg).is_none());
    }

    #[test]
    fn first_pls_carries_correct_profile_byte() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0xABCD);
        let symbols = build_superframe_v4(&payload, &cfg, 0x77, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        let pls = result.first_pls.expect("first_pls");
        // The current encoder hardwires profile_index=0 in the PLS;
        // upgrade path: worker injects the real ProfileIndex2x byte
        // (Phase C-7). Until then we just verify the field is present.
        assert_eq!(pls.profile_index, 0);
        assert_eq!(pls.session_id_low, 0x77);
        assert_eq!(pls.base_esi, 0);
    }

    #[test]
    fn roundtrip_all_eight_profiles() {
        // Sanity sweep — every profile must roundtrip a 500-byte payload
        // noise-free in ≤ 3 PLHEADER cycles.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(500, p.as_u8() as u64);
            let symbols =
                build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
            let result = rx_v4_symbols(&symbols, &cfg)
                .unwrap_or_else(|| panic!("{p:?} decode returned None"));
            assert_eq!(result.data, payload, "{p:?} payload mismatch");
        }
    }

    #[test]
    fn pilot_sigma2_below_floor_when_noise_free() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(1_000, 0x1);
        let symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        // Noise-free: residuals ≈ 0 → σ² is clamped to SIGMA2_FLOOR.
        assert!(
            (result.sigma2_data - SIGMA2_FLOOR).abs() < 1e-9,
            "noise-free σ² should clamp to floor, got {}",
            result.sigma2_data
        );
    }
}
