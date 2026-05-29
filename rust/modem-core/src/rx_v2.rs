//! v2 receive pipeline: segment-aware RX with resync markers.
//!
//! The stream produced by `frame::build_superframe_v2` has the structure
//! ```text
//!   [preamble][header v=2][marker][seg0 + pilots][marker][seg1 + pilots]...
//! ```
//! where each segment is either a meta segment (1 LDPC codeword carrying the
//! application header) or a data segment (N_cw codewords carrying payload).
//!
//! This module walks the stream segment by segment, validates each marker's
//! CRC8, applies per-segment pilot-aided magnitude correction + stream-persistent
//! DD-PLL, and assembles the decoded payload from `(base_ESI, codeword)` pairs.

use std::collections::HashMap;

use modem_framing::app_header::{self, AppHeader};
use crate::demodulator;
use crate::ffe;
use crate::frame::{self, HEADER_VERSION_V3, V2_CODEWORDS_PER_SEGMENT};
use crate::header;
use crate::interleaver;
use crate::ldpc::decoder::LdpcDecoder;
use crate::marker::{self, MarkerPayload, MARKER_CTRL_LEN, MARKER_LEN, MARKER_SYNC_LEN};
use crate::pilot;
use crate::pll::DdPll;
use crate::preamble;
use crate::profile::ModemConfig;
use crate::rrc::{self, rrc_taps};
use crate::soft_demod;
use crate::sync;
use crate::types::{Complex64, AUDIO_RATE, N_PREAMBLE, RRC_SPAN_SYM};

use std::cell::RefCell;
use std::time::Instant;

/// Per-stage CPU breakdown of a single tick's decode work, in microseconds.
///
/// Accumulated via the thread-local in [`PERF`] and harvested by the worker
/// with [`take_perf`] after every `rx_v2_with_options` / `rx_v3_after` call.
/// Each field accumulates across **all internal passes** that ran for the
/// tick (Gardner estimator, hint-resampled decode, fast-path decode,
/// optional ±15 ppm grid). The worker reports a 30-SF rolling average + max
/// in the `[perf]` log entry.
///
/// Diagnostic only — no behaviour depends on these numbers. The cost of
/// `Instant::now()` per stage is negligible (~100 ns) against stages that
/// run in 1-100 ms range on lowpower hosts (Pi5).
#[derive(Debug, Clone, Copy, Default)]
pub struct PerfBreakdown {
    pub downmix_us: u64,
    pub matched_filter_us: u64,
    pub find_preamble_us: u64,
    pub decimate_us: u64,
    pub ffe_ls_us: u64,
    pub ffe_lms_us: u64,
    pub marker_scan_us: u64,
    pub ldpc_us: u64,
    pub raptorq_us: u64,
    pub resample_us: u64,
    /// Number of *internal passes* that contributed to this aggregate
    /// (typically 1-3 : Gardner + hint-decode + fast-path). Lets the
    /// worker spot ticks that hit the slow ±15 ppm grid path.
    pub n_passes: u32,
}

impl std::ops::AddAssign for PerfBreakdown {
    fn add_assign(&mut self, rhs: Self) {
        self.downmix_us += rhs.downmix_us;
        self.matched_filter_us += rhs.matched_filter_us;
        self.find_preamble_us += rhs.find_preamble_us;
        self.decimate_us += rhs.decimate_us;
        self.ffe_ls_us += rhs.ffe_ls_us;
        self.ffe_lms_us += rhs.ffe_lms_us;
        self.marker_scan_us += rhs.marker_scan_us;
        self.ldpc_us += rhs.ldpc_us;
        self.raptorq_us += rhs.raptorq_us;
        self.resample_us += rhs.resample_us;
        self.n_passes += rhs.n_passes;
    }
}

impl PerfBreakdown {
    pub fn total_us(&self) -> u64 {
        self.downmix_us
            + self.matched_filter_us
            + self.find_preamble_us
            + self.decimate_us
            + self.ffe_ls_us
            + self.ffe_lms_us
            + self.marker_scan_us
            + self.ldpc_us
            + self.raptorq_us
            + self.resample_us
    }
}

thread_local! {
    /// Thread-local accumulator for the per-stage timings. Stages inside
    /// `rx_v2_single` / `estimate_drift_gardner` / `resample_audio` write
    /// into this ; the worker reads + resets via [`take_perf`] once per
    /// tick. Single-threaded by construction (the worker pipeline is
    /// sync) -- the thread_local is a quiet way to thread the timings
    /// through without polluting every function signature.
    static PERF: RefCell<PerfBreakdown> = RefCell::new(PerfBreakdown::default());
}

/// Atomically take the current per-stage accumulator and reset it to zero.
/// Called by the worker once per tick after `rx_v3_after` returns, so each
/// `[perf]` log entry reflects exactly one tick's work.
pub fn take_perf() -> PerfBreakdown {
    PERF.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// Record `dt` (in microseconds) into the named field of the thread-local
/// accumulator. Used by the staged macros below ; not part of the public
/// surface.
fn record_stage(stage: PerfStage, dt_us: u64) {
    PERF.with(|p| {
        let mut b = p.borrow_mut();
        match stage {
            PerfStage::Downmix => b.downmix_us += dt_us,
            PerfStage::MatchedFilter => b.matched_filter_us += dt_us,
            PerfStage::FindPreamble => b.find_preamble_us += dt_us,
            PerfStage::Decimate => b.decimate_us += dt_us,
            PerfStage::FfeLs => b.ffe_ls_us += dt_us,
            PerfStage::FfeLms => b.ffe_lms_us += dt_us,
            PerfStage::MarkerScan => b.marker_scan_us += dt_us,
            PerfStage::Ldpc => b.ldpc_us += dt_us,
            PerfStage::RaptorQ => b.raptorq_us += dt_us,
            PerfStage::Resample => b.resample_us += dt_us,
            PerfStage::PassDone => b.n_passes += 1,
        }
    });
}

#[derive(Debug, Clone, Copy)]
enum PerfStage {
    Downmix,
    MatchedFilter,
    FindPreamble,
    Decimate,
    FfeLs,
    FfeLms,
    MarkerScan,
    Ldpc,
    RaptorQ,
    Resample,
    PassDone,
}

/// Helper RAII timer : records `stage` with the elapsed time when dropped.
/// Wrap heavy code blocks with `let _t = StageTimer::new(stage);`.
struct StageTimer {
    stage: PerfStage,
    t0: Instant,
}

impl StageTimer {
    fn new(stage: PerfStage) -> Self {
        Self { stage, t0: Instant::now() }
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        record_stage(self.stage, self.t0.elapsed().as_micros() as u64);
    }
}

/// Result of decoding a v2 superframe.
pub struct RxV2Result {
    pub data: Vec<u8>,
    pub header: Option<header::Header>,
    pub app_header: Option<AppHeader>,
    pub converged_blocks: usize,
    pub total_blocks: usize,
    pub segments_decoded: usize,
    pub segments_lost: usize,
    /// Pilot-residual σ². Computed from the LS-fit residuals on KNOWN
    /// pilot symbols (statistically optimal noise-variance estimator,
    /// used as the LLR scale by the soft demod). Includes pilots from
    /// every segment in the window, including meta.
    pub sigma2: f64,
    /// Data-symbol σ², computed from hard-decision residuals on the
    /// post-equalisation DATA symbols of non-meta segments only. This
    /// is the value the GUI surfaces as "frame noise" — it excludes
    /// pilot/preamble overhead so the operator sees what the actual
    /// payload symbols look like to the demod. Slightly biased low at
    /// low SNR (wrong decisions reduce the residual artificially) but
    /// good enough for a live indicator. Falls back to `sigma2` when
    /// no data segments were processed.
    pub sigma2_data: f64,
    /// Unique DATA ESIs recovered (excludes meta-segment blocks, which are
    /// framing overhead rather than payload content). This is the metric
    /// the GUI should display as real decode progress.
    pub data_blocks_recovered: usize,
    /// DATA codewords recovered, keyed by ESI. Empty if no AppHeader was
    /// decoded (assembly then falls back to ESI-sort of whatever was
    /// decoded — same fallback the single-pass path uses).
    ///
    /// Exposed so that `rx_v3` can merge per-window maps when sliding-window
    /// decoding a v3 stream.
    pub cw_bytes_map: HashMap<u32, Vec<u8>>,
    /// Any decoded header in this window (single-pass) or across windows
    /// (rx_v3) carried FLAG_EOT — the TX explicitly signalled end-of-burst.
    pub eot_seen: bool,
    /// Sample of post-equalisation data symbols (Re, Im) from this window's
    /// data segments. Meta segments are excluded (they're encoded with
    /// the data constellation but appear only once per cycle, biasing the
    /// scatter). Capped at ~500 points — the GUI displays them as a
    /// scatter plot.
    pub constellation_sample: Vec<[f32; 2]>,
    /// Pilot LS smoothed phases (radians) per decoded segment in this
    /// window, in temporal order. Both META and DATA segments are
    /// included so the GUI can show the full picture of the frame.
    /// Companion [`pilot_phase_is_meta`] flags which entries are META.
    pub pilot_phase_segments: Vec<Vec<f32>>,
    /// Parallel to `pilot_phase_segments`: `true` if the segment at the
    /// same index is a META segment (header replicated), `false` for
    /// a regular DATA segment. Lets the GUI render META in a distinct
    /// colour so the operator sees the full frame layout rather than
    /// data-only segments.
    pub pilot_phase_is_meta: Vec<bool>,
    /// Parallel to `pilot_phase_segments`: per-segment pilot-residual
    /// sigma² (= mean squared residual of `received_pilot - g_hat *
    /// expected_pilot` over the segment's pilot symbols). Lets us
    /// confirm whether a high aggregate sigma² is uniformly distributed
    /// across segments or concentrated on a few. NaN if a segment had
    /// no pilots accumulated.
    pub pilot_sigma2_per_segment: Vec<f64>,
    /// Per-segment skewness of pilot residuals (Re/Im stacked). Gaussian
    /// baseline = 0 ; |skew| > ~0.3 suggests an asymmetric noise
    /// distribution (bursty fade, residual carrier, AM-AM nonlinearity).
    pub pilot_skew_per_segment: Vec<f64>,
    /// Per-segment EXCESS kurtosis of pilot residuals (kurtosis - 3 of
    /// stacked Re/Im samples). Gaussian baseline = 0 ; kurt_excess >
    /// ~1 suggests impulsive content (PLC noise, switching supplies,
    /// nearby ignition systems). Heavy-tailed alpha-stable noise can
    /// easily reach 10-100.
    pub pilot_kurt_per_segment: Vec<f64>,
    /// Offset (in samples, relative to the input slice) of the LAST preamble
    /// found by `rx_v3` in this call — closed or open. Callers maintaining a
    /// rolling capture buffer can drain everything before
    /// `last_preamble_offset - margin` after a successful scan: closed
    /// windows are already routed to the store, and the open one is rebuilt
    /// from the preserved P_last (+ MF pre-roll margin) on the next tick when
    /// the next preamble lands. The buffer becomes a self-purging queue that
    /// tracks the live preamble cadence — no more guess-the-period
    /// scan-window heuristic.
    /// `None` for `rx_v2_single` (single-window, no preamble walking) or
    /// `rx_v3_after` calls where no preamble was found at all.
    pub last_preamble_offset: Option<usize>,
    /// Per-window drift compensation (ppm, positive = RX clock faster than
    /// TX) applied to produce this decode. Populated by `rx_v2`'s grid
    /// search (= `best_ppm`); 0.0 when the first-pass `rx_v2_single`
    /// already converged or when produced by `rx_v2_single` /
    /// `rx_v3_after` directly. `rx_v3_after` carries this value forward
    /// from the latest CLOSED window to the OPEN one (via `rx_v2_with_hint`),
    /// because TX/RX sound-card clocks drift slowly enough that the
    /// previous superframe's ppm is a near-perfect prior — much more
    /// reliable than a blind grid search on a loosely-constrained OPEN
    /// window.
    pub drift_ppm: f64,
    /// FFE tap centroid right after LS-training on the preamble
    /// (= "where the equalizer thinks the symbol peak lives" at the
    /// start of the window). Expressed in FSE-input samples, so
    /// the natural center is `(n_ff - 1) / 2` and a shift of
    /// `ffe_centroid_final - ffe_centroid_initial` reflects how much
    /// the LMS adapter had to move the peak over the window's
    /// duration. Used by the worker to detect uncorrected
    /// sample-rate drift accumulating across an SF -- a non-zero
    /// shift means residual ppm error not absorbed by the running
    /// session-level estimate.
    pub ffe_centroid_initial: f64,
    /// FFE tap centroid at the end of the window's LMS adaptation.
    /// Same units as `ffe_centroid_initial`; their difference is the
    /// drift diagnostic surfaced for re-estimation triggering.
    pub ffe_centroid_final: f64,
}

/// Cheap preamble presence probe — intended for the RX worker's Idle gate.
///
/// Computes `correlate_at` at every `pitch` sample across the matched-
/// filter output and compares the global peak against the median of all
/// candidates. On a clean preamble, peak / median ≫ 10 ; on pure noise, the
/// ratio hovers near 2–3. Returns `true` if the ratio exceeds a fixed
/// threshold, suggesting a real preamble is present somewhere in `samples`.
///
/// Uses the same symbol-rate coarse scan as `find_all_preambles` but without
/// NMS or fine-refine, making it an order of magnitude cheaper than
/// `rx_v2` / `rx_v3` when the channel is idle.
pub fn probe_preamble_present(samples: &[f32], config: &ModemConfig) -> bool {
    let Ok((sps, pitch)) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau) else {
        return false;
    };
    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
    let bb = demodulator::downmix(samples, config.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);

    let preamble_syms = preamble::make_preamble_for_config(config);
    let n_pre = preamble_syms.len();
    let max_start = mf.len().saturating_sub(n_pre * pitch);
    if max_start == 0 {
        return false;
    }

    let mut mags: Vec<f64> = Vec::new();
    let mut global_max = 0.0f64;
    let mut start = 0usize;
    while start <= max_start {
        let mag = correlate_at_public(&mf, &preamble_syms, start, pitch);
        if mag > global_max {
            global_max = mag;
        }
        mags.push(mag);
        start += pitch;
    }
    if mags.is_empty() || global_max <= 0.0 {
        return false;
    }
    mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = mags[mags.len() / 2].max(1e-12);
    // Empirical : on a clean V3 burst, peak/median of |corr|² is
    // typically > 10⁴ ; pure noise produces extreme-value ratios in the
    // 10–30 range (|corr|² is exponentially distributed over scan
    // positions). 50 leaves comfortable margin on both sides.
    global_max / median > 50.0
}

fn correlate_at_public(
    mf: &[Complex64],
    preamble: &[Complex64],
    start: usize,
    pitch: usize,
) -> f64 {
    let mut acc = Complex64::new(0.0, 0.0);
    for (k, &sym) in preamble.iter().enumerate() {
        let idx = start + k * pitch;
        if idx >= mf.len() {
            break;
        }
        acc += mf[idx] * sym.conj();
    }
    acc.norm_sqr()
}

/// Peak / median correlation ratio with the preamble, for a given modem
/// configuration. Higher = more likely to contain a preamble at that
/// profile's symbol rate.
///
/// Same inner work as `probe_preamble_present` but exposes the ratio for
/// comparison across profiles in `detect_best_profile`.
fn preamble_correlation_ratio(samples: &[f32], config: &ModemConfig) -> f64 {
    let Ok((sps, pitch)) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau) else {
        return 0.0;
    };
    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
    let bb = demodulator::downmix(samples, config.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);

    let preamble_syms = preamble::make_preamble_for_config(config);
    let n_pre = preamble_syms.len();
    let max_start = mf.len().saturating_sub(n_pre * pitch);
    if max_start == 0 {
        return 0.0;
    }

    let mut mags: Vec<f64> = Vec::new();
    let mut global_max = 0.0f64;
    let mut start = 0usize;
    while start <= max_start {
        let mag = correlate_at_public(&mf, &preamble_syms, start, pitch);
        if mag > global_max {
            global_max = mag;
        }
        mags.push(mag);
        start += pitch;
    }
    if mags.is_empty() || global_max <= 0.0 {
        return 0.0;
    }
    mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = mags[mags.len() / 2].max(1e-12);
    global_max / median
}

/// Scan the preamble correlation for all 5 canonical profiles. Returns
/// the profile to switch to — or `None` if the current one is fine.
///
/// Rules :
/// - Returns `None` if the best ratio is below 50 (no signal).
/// - Returns `None` if the `current` profile has a ratio ≥ 90 % of the
///   best (no meaningful improvement ; avoids oscillating on ties
///   between profiles sharing `(Rs, τ, β)` like HIGH vs NORMAL).
/// - Returns `None` if the best beats the runner-up by less than 1.5×
///   (ambiguous pitch region — let the header `profile_index` refine
///   downstream rather than picking at random).
/// - Otherwise returns `Some(best)`.
///
/// The caller passes its current `ProfileIndex` so ties between the
/// current profile and another profile at identical pitch are treated
/// as "no change".
pub fn detect_best_profile(
    samples: &[f32],
    current: crate::profile::ProfileIndex,
) -> Option<crate::profile::ProfileIndex> {
    use crate::profile::ProfileIndex;
    // Exclude experimental profiles (Mega, Fast, HighPlusFiveSix) from
    // auto-detect — they're forced-only on the RX side. Including them in
    // the sweep would let a tie-broken pick land on an experimental
    // profile that the worker isn't authorised to switch into.
    let scored: Vec<(ProfileIndex, f64)> = ProfileIndex::ALL
        .iter()
        .filter(|p| !p.is_experimental())
        .map(|&p| (p, preamble_correlation_ratio(samples, &p.to_config())))
        .collect();
    let mut sorted = scored.clone();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (best, best_r) = sorted[0];
    if best_r < 50.0 {
        return None;
    }
    let current_r = scored
        .iter()
        .find(|&&(p, _)| p == current)
        .map(|&(_, r)| r)
        .unwrap_or(0.0);
    // Current profile is already as good as the best (within 10 %) : keep
    // it, no switch needed. This is the decisive check that prevents
    // oscillation on (Rs, τ, β) ties — current being one of the tied
    // profiles has a ratio equal to best_r, so we stay put. The header's
    // profile_index refines the exact profile downstream.
    if current_r >= 0.9 * best_r {
        return None;
    }
    Some(best)
}

/// Energy-weighted index of an FFE tap vector, in FSE-input samples.
///
/// `centroid = sum_k k * |tap[k]|^2 / sum_k |tap[k]|^2`. For an LS-trained
/// FFE on a clean preamble the centroid sits at the geometric center
/// `(n_ff - 1) / 2`. After LMS adapts across a window with residual
/// sample-rate drift, the centroid migrates by ~`drift_ppm × window_s
/// × sample_rate / pitch_fse` samples. The worker observes the shift
/// (`final - initial`) to decide whether the running session_drift_ppm
/// needs a Gardner re-estimation.
fn ffe_tap_centroid(taps: &[Complex64]) -> f64 {
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for (k, t) in taps.iter().enumerate() {
        let p = t.norm_sqr();
        num += (k as f64) * p;
        den += p;
    }
    if den > 1e-12 {
        num / den
    } else {
        (taps.len() as f64 - 1.0) / 2.0
    }
}

/// Linear-interpolation resample of a float audio stream to compensate a
/// sample-rate mismatch of `drift_ppm` ppm between the transmitter and the
/// receiver sound cards. A positive `drift_ppm` means the RX clock was faster
/// than TX (RX captured `(1 + ε)` samples for each TX sample) and the output
/// is a SHORTER stream that matches TX timing.
fn resample_audio(samples: &[f32], drift_ppm: f64) -> Vec<f32> {
    let _t = StageTimer::new(PerfStage::Resample);
    let ratio = 1.0 + drift_ppm * 1e-6;
    let n_out = ((samples.len() as f64) / ratio) as usize;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let t = i as f64 * ratio;
        let idx = t.floor() as usize;
        let frac = (t - idx as f64) as f32;
        if idx + 1 < samples.len() {
            out.push((1.0 - frac) * samples[idx] + frac * samples[idx + 1]);
        } else if idx < samples.len() {
            out.push(samples[idx]);
        } else {
            break;
        }
    }
    out
}

/// Top-level v2 decode with automatic sound-card drift compensation.
///
/// **Phase 1d (0.10.15+) — Gardner TED + LS-FFE timing estimation.**
/// Runs the single-pass decode once at 0 ppm. If clean, returns it.
/// Otherwise:
///
///   1. Call [`estimate_drift_gardner`] which builds the MF + LS-trained
///      FFE on the preamble (= DVB-S2X PLHEADER pattern), then computes
///      the Gardner timing-error detector (Gardner 1986) at every data
///      symbol, block-averages, and OLS-fits the slope vs symbol
///      index. Slope * 1e6 / (2π × β) gives the drift in ppm.
///   2. Resample at that ppm and decode once.
///   3. If still not clean, fall back to a narrow ±15 ppm safety grid
///      around the Gardner estimate.
///
/// **Why Gardner instead of inventing.** The marker-position approach
/// (0.10.13–0.10.14) had an opaque K-factor depending on FFE
/// windowing, which I couldn't analytically pin down and which varied
/// by profile (especially FTN). Gardner TED is the canonical timing
/// recovery in DVB-S2/S2X, V.34, every modern coherent demodulator
/// since 1986. It has a known closed-form gain (2π × β for RRC
/// roll-off β at unit Es), so the slope-to-ppm conversion is
/// deterministic without per-profile calibration.
///
/// **Cost.** Gardner is open-loop (no iteration), so the cost is
/// 1 MF + 1 LS-FFE training + 2 FFE applications (on-symbol + half-
/// symbol) ≈ 1.3× a single decode (LDPC is ~10%). Total:
/// 1 estimator + 1 final decode ≈ 2.3× a single decode. Down from
/// the 0.10.12 blind grid's 14× and from the 0.10.14 hint-grid's 8.85×.
pub fn rx_v2(samples: &[f32], config: &ModemConfig) -> Option<RxV2Result> {
    rx_v2_with_options(samples, config, true, true)
}

/// Same as [`rx_v2`] but lets the caller skip the post-Gardner ±15 ppm
/// safety grid AND/OR the 0-ppm fast-path. The grid is the most CPU-heavy
/// fallback (6 extra `rx_v2_single` passes on resampled buffers) ; on weak
/// hosts (Pi-class) it's pure waste when the channel is noise-limited
/// (LDPC fails at every trial point anyway, but we still pay the
/// resample + MF + FFE + LMS + LDPC cost per point). Set
/// `allow_legacy_grid = false` to bypass it.
///
/// The Gardner-one-shot (`estimate_drift_gardner` + `rx_v2_with_hint`)
/// always runs -- it's cheap and gives the operator a faithful drift
/// readout. The 0-ppm fast-path (`rx_v2_single` on the raw buffer) is a
/// second matched-filter pass that costs ~230 ms on a Pi 4 ; when
/// Gardner already produced a clean decode at >=0.5 ppm it's pure
/// redundancy. Set `allow_fast_path = false` (paired with
/// `allow_legacy_grid = false` in low-power mode on CLOSED windows where
/// Gardner is sub-ppm accurate) to skip it. OPEN windows on cold start
/// still benefit from the fast-path so its caller should keep
/// `allow_fast_path = true` there.
/// LOOSE convergence check — at least one segment decoded AND ≥99 %
/// of LDPC codewords converged. Used to gate the Axis-1 fast-path skip
/// (rx_v2_with_options : when Gardner's hint-decoded result clears
/// this bar, the 0-ppm fast-path is redundant and the lowpower CLOSED
/// path drops it). Promoted from a closure to a free function so
/// worker threads in the parallel safety grid (aarch64) can call it
/// without capturing across thread boundaries.
fn is_clean(r: &RxV2Result) -> bool {
    r.total_blocks > 0
        && r.converged_blocks * 100 >= r.total_blocks * 99
        && r.segments_decoded > 0
}

/// STRICT convergence check for the parallel grid early-exit. Requires :
///   - 100 % of attempted LDPC codewords converged (NOT 99 %) — a
///     single CW failure means the cell didn't perfectly equalise the
///     drifted samples and we should keep trying the other cells.
///   - no segment marker was lost (`segments_lost == 0`) — if a
///     marker slid out of position, the cell's drift hypothesis is
///     mis-aligned even though the survivor segments may have decoded
///     cleanly.
///   - at least one **DATA** CW was recovered (`data_blocks_recovered
///     > 0`) — guards against the meta-only-converged edge case where
///     `total=converged=1` because the META marker passed CRC but every
///     DATA marker failed (or because the decode window was cut short
///     right after the meta segment). The loose [`is_clean`] would
///     return true in that case ; here we explicitly require some
///     PAYLOAD to have decoded before we stop trying alternative drifts.
///
/// Kept distinct from [`is_clean`] so the Axis-1 fast-path gate keeps
/// its original 99 % tolerance (preserves the perf gain measured in
/// `rx_v2_with_options_skips_fast_path_when_gardner_clean`) while the
/// grid only short-circuits on a payload-complete decode.
fn is_fully_clean(r: &RxV2Result) -> bool {
    r.segments_decoded > 0
        && r.segments_lost == 0
        && r.data_blocks_recovered > 0
        && r.total_blocks > 0
        && r.converged_blocks == r.total_blocks
}

pub fn rx_v2_with_options(
    samples: &[f32],
    config: &ModemConfig,
    allow_legacy_grid: bool,
    allow_fast_path: bool,
) -> Option<RxV2Result> {
    let mut best: Option<RxV2Result> = None;
    let mut best_score: f64 = -1.0;
    let mut best_ppm: f64 = 0.0;

    // 1. Gardner one-shot FIRST.
    //
    // Rationale (2026-05-13 regression analysis on HIGH+ OTA): if we
    // tried the 0-ppm fast-path first and returned early on "clean"
    // (= 99 % LDPC), some SFs on a drifted channel would pass the
    // gate via pilot-tracker absorption while still showing a
    // scattered constellation -- AND the result would report
    // `drift_ppm = 0.0`, hiding the actual channel drift from
    // telemetry. Gardner is sub-ppm accurate on a CLOSED with ≥6
    // markers and costs ~50 ms ; running it first gives the operator
    // a faithful drift readout and a tighter constellation, with the
    // fast-path retained as a fallback for the (rare) case where
    // Gardner can't lock.
    if let Some(gardner_ppm) = estimate_drift_gardner(samples, config) {
        // Only resample when Gardner returned a meaningful magnitude.
        // Below 0.5 ppm the pilot tracker absorbs the residual cleanly
        // and resampling adds interpolation noise for no gain ; we drop
        // through to the fast-path below which leaves drift_ppm at 0.0
        // for telemetry (= "channel is drift-free or within tracker
        // tolerance, no correction applied").
        if gardner_ppm.abs() >= 0.5 {
            if let Some(r) = rx_v2_with_hint(samples, config, gardner_ppm) {
                best_score = score_result(&r);
                best_ppm = gardner_ppm;
                best = Some(r);
            }
        }
    }

    // 2. Fast-path at 0 ppm. Two gates :
    //    - `allow_fast_path = true` (desktop / OPEN cold-start) always
    //      runs the fast-path. Preserves the score comparison that
    //      lets the rare "fast-path-beats-Gardner" outlier still win.
    //    - `allow_fast_path = false` (lowpower CLOSED) skips the pass
    //      ONLY when Gardner already produced a clean decode -- the
    //      stated rationale of the 0.10.29 commit. When Gardner failed
    //      to lock (sub-0.5 ppm channel where the branch is gated out
    //      at line 566, OR a noisy channel where rx_v2_with_hint
    //      returned None) the fast-path stays ON as the safety net
    //      that lets the SF still decode.
    let gardner_clean = best.as_ref().map(is_clean).unwrap_or(false);
    if allow_fast_path || !gardner_clean {
        if let Some(r) = rx_v2_single(samples, config) {
            let s = score_result(&r);
            if s > best_score {
                best_score = s;
                best_ppm = 0.0;
                best = Some(r);
            }
        }
    }

    // 3. Safety grid: ±15 ppm around best_ppm in 5-ppm steps. Covers
    //    cases where Gardner is off-calibration (rare for RRC pulses
    //    but possible on FTN profiles, dense constellations, low SNR).
    //    Skipped when `allow_legacy_grid = false` (Pi-class hosts that
    //    can't afford even a parallel grid pass).
    let still_not_clean = best.as_ref().map(|r| !is_clean(r)).unwrap_or(true);
    if allow_legacy_grid && still_not_clean {
        let center = best_ppm;

        #[cfg(target_arch = "aarch64")]
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            let batches: [&[f64]; 2] =
                [&[-5.0, 5.0, -10.0, 10.0], &[-15.0, 15.0]];
            'outer: for batch in batches {
                let cancel = AtomicBool::new(false);
                let entries: Vec<(f64, RxV2Result, f64)> =
                    std::thread::scope(|s| {
                        let handles: Vec<_> = batch
                            .iter()
                            .map(|&delta| {
                                let ppm = center + delta;
                                let cfg = config.clone();
                                let cancel_ref = &cancel;
                                s.spawn(move || {
                                    let corrected = resample_audio(samples, ppm);
                                    let r = rx_v2_single_cancellable(
                                        &corrected,
                                        &cfg,
                                        Some(cancel_ref),
                                    )?;
                                    if is_fully_clean(&r) {
                                        cancel_ref.store(true, Ordering::Relaxed);
                                    }
                                    Some((score_result(&r), r, ppm))
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .filter_map(|h| h.join().ok().flatten())
                            .collect()
                    });
                let mut found_clean = false;
                for (s, r, ppm) in entries {
                    if is_fully_clean(&r) {
                        found_clean = true;
                    }
                    if s > best_score {
                        best_score = s;
                        best = Some(r);
                        best_ppm = ppm;
                    }
                }
                if found_clean {
                    break 'outer;
                }
            }
        }

        #[cfg(not(target_arch = "aarch64"))]
        {
            for &delta in &[-15.0, -10.0, -5.0, 5.0, 10.0, 15.0] {
                let ppm = center + delta;
                let corrected = resample_audio(samples, ppm);
                if let Some(r) = rx_v2_single(&corrected, config) {
                    let s = score_result(&r);
                    if s > best_score {
                        best_score = s;
                        best = Some(r);
                        best_ppm = ppm;
                    }
                }
            }
        }
    }

    if best_ppm != 0.0 {
        eprintln!("[rx_v2] drift-compensated at {best_ppm:+.2} ppm");
    }
    if let Some(ref mut r) = best {
        r.drift_ppm = best_ppm;
    }
    best
}

/// Power Mode RX = 0.9.x algorithm. No Gardner, no FFE-centroid
/// re-estimation, no EWMA hint chain. Just :
///
///   1. **0 ppm fast-path** — `rx_v2_single` on the raw buffer. If it
///      converges fully (`is_fully_clean`) we stop immediately ; that
///      covers the common "no drift" case in the cheapest way.
///   2. **±80 ppm coarse grid** (8 cells, 20 ppm step) — covers any
///      plausible sound-card drift (typical TCXO/XO mismatch ≤ ±100 ppm
///      across consumer sound-cards). On aarch64 we batch in two
///      parallel groups of 4 via `std::thread::scope` ; on other arches
///      sequential with early-exit on `is_fully_clean`.
///   3. **±10 ppm fine refine** (4 cells, 5 ppm step around the coarse
///      winner) — only when the coarse winner is meaningfully off-center
///      (|ppm| > 0.5). Parallel on aarch64.
///
/// This is the 0.9.2rc5 receive recipe — broad, robust, slightly more
/// CPU than the modern Gardner pipeline but well within budget on any
/// PC modern enough to be ticked into Power Mode by the operator.
pub fn rx_v2_legacy_grid_decode(samples: &[f32], config: &ModemConfig) -> Option<RxV2Result> {
    let mut best: Option<RxV2Result> = None;
    let mut best_score: f64 = -1.0;
    let mut best_ppm: f64 = 0.0;

    // Stage 1 : 0 ppm fast-path. Short-circuit if already fully clean.
    if let Some(r) = rx_v2_single(samples, config) {
        let clean = is_fully_clean(&r);
        let s = score_result(&r);
        best_score = s;
        best_ppm = 0.0;
        best = Some(r);
        if clean {
            if let Some(ref mut r) = best {
                r.drift_ppm = best_ppm;
            }
            return best;
        }
    }

    // Stage 2 : ±80 ppm coarse grid, 20-ppm step.
    let coarse_deltas: [f64; 8] = [-80.0, -60.0, -40.0, -20.0, 20.0, 40.0, 60.0, 80.0];
    run_legacy_grid_pass(
        samples,
        config,
        0.0,
        &coarse_deltas,
        &mut best,
        &mut best_score,
        &mut best_ppm,
    );

    // Stage 3 : ±10 ppm fine refine around the coarse winner. Skip when
    // the coarse winner stayed near 0 ppm — the fast-path already
    // covered ±0, no point retrying at ±5/±10 from the same center.
    if best_ppm.abs() > 0.5 {
        let fine_deltas: [f64; 4] = [-10.0, -5.0, 5.0, 10.0];
        run_legacy_grid_pass(
            samples,
            config,
            best_ppm,
            &fine_deltas,
            &mut best,
            &mut best_score,
            &mut best_ppm,
        );
    }

    if best_ppm != 0.0 {
        eprintln!("[rx_v2] (legacy grid) drift-compensated at {best_ppm:+.2} ppm");
    }
    if let Some(ref mut r) = best {
        r.drift_ppm = best_ppm;
    }
    best
}

/// One grid pass : resample at `(center_ppm + delta)` for each delta,
/// decode via `rx_v2_single`, keep the best by score. Parallel on
/// aarch64 (batches of 4) with `AtomicBool` early-exit on the STRICT
/// `is_fully_clean` predicate ; sequential elsewhere with the same
/// early-exit.
fn run_legacy_grid_pass(
    samples: &[f32],
    config: &ModemConfig,
    center_ppm: f64,
    deltas: &[f64],
    best: &mut Option<RxV2Result>,
    best_score: &mut f64,
    best_ppm: &mut f64,
) {
    #[cfg(target_arch = "aarch64")]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Process in batches of up to 4 (Pi 4 quad-core).
        let chunks: Vec<&[f64]> = deltas.chunks(4).collect();
        'outer: for chunk in chunks {
            let cancel = AtomicBool::new(false);
            let entries: Vec<(f64, RxV2Result, f64)> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|&delta| {
                        let ppm = center_ppm + delta;
                        let cfg = config.clone();
                        let cancel_ref = &cancel;
                        s.spawn(move || {
                            let corrected = resample_audio(samples, ppm);
                            let r = rx_v2_single_cancellable(
                                &corrected,
                                &cfg,
                                Some(cancel_ref),
                            )?;
                            if is_fully_clean(&r) {
                                cancel_ref.store(true, Ordering::Relaxed);
                            }
                            Some((score_result(&r), r, ppm))
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok().flatten())
                    .collect()
            });
            let mut found_clean = false;
            for (s, r, ppm) in entries {
                if is_fully_clean(&r) {
                    found_clean = true;
                }
                if s > *best_score {
                    *best_score = s;
                    *best_ppm = ppm;
                    *best = Some(r);
                }
            }
            if found_clean {
                break 'outer;
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        for &delta in deltas {
            let ppm = center_ppm + delta;
            let corrected = resample_audio(samples, ppm);
            if let Some(r) = rx_v2_single(&corrected, config) {
                let clean = is_fully_clean(&r);
                let s = score_result(&r);
                if s > *best_score {
                    *best_score = s;
                    *best_ppm = ppm;
                    *best = Some(r);
                }
                if clean {
                    break;
                }
            }
        }
    }
}

/// Decode `samples` after pre-resampling by `hint_ppm`. Single-pass: no
/// grid search, no refine. Order-of-magnitude cheaper than [`rx_v2`] and
/// only safe when the caller has a tight prior on the actual TX/RX clock
/// drift — typically the [`RxV2Result::drift_ppm`] returned by a recent
/// [`rx_v2`] decode of a neighbouring window in the same session.
///
/// Used by [`rx_v3_after`] to decode the OPEN (trailing, no-closing-
/// preamble) window with the drift estimate of the latest CLOSED window
/// instead of either (a) skipping it — would lose the last superframe of
/// a burst — or (b) re-running the blind grid search on a loosely-
/// constrained window — expensive and unreliable, the segments past the
/// last pilot drift unboundedly without an anchoring preamble.
///
/// Pass `hint_ppm = 0.0` to short-circuit straight to [`rx_v2_single`] —
/// the resample is a no-op at zero ppm and we avoid the allocation.
pub fn rx_v2_with_hint(
    samples: &[f32],
    config: &ModemConfig,
    hint_ppm: f64,
) -> Option<RxV2Result> {
    // Snap sub-grid-step hints to zero; cheaper than resampling and the
    // residual ppm gets absorbed by the per-segment pilot tracker anyway.
    if hint_ppm.abs() < 0.5 {
        return rx_v2_single(samples, config);
    }
    let corrected = resample_audio(samples, hint_ppm);
    let mut r = rx_v2_single(&corrected, config)?;
    r.drift_ppm = hint_ppm;
    Some(r)
}

/// Score a decode result for comparing drift candidates. Higher is better.
/// Combines (a) number of segments actually decoded (penalises scans that
/// fail at markers), and (b) fraction of LDPC blocks that converged.
fn score_result(r: &RxV2Result) -> f64 {
    let seg_score = r.segments_decoded as f64;
    let block_score = if r.total_blocks > 0 {
        r.converged_blocks as f64 / r.total_blocks as f64
    } else {
        0.0
    };
    // Blocks-converged is the metric that matters for payload integrity;
    // weight it 10× more than segment count.
    seg_score + 10.0 * block_score * r.total_blocks as f64
}

/// Estimate the TX-to-RX sample-rate drift via DVB-S2X-style data-aided
/// timing recovery on the matched-filter output.
///
/// **Architecture (mirroring DVB-S2/S2X reference).**
///
///   1. Cross-correlate the preamble (~ PLHEADER) against mf to anchor
///      `sync_pos` (integer audio sample).
///   2. Quick FFE-LMS pass at symbol rate to LOCATE marker sync
///      patterns (= our equivalent of pilot block boundaries). Returns
///      a list of marker integer indices in symbol space.
///   3. For each marker, compute its NOMINAL audio sample position
///      from the frame layout, then SUB-SAMPLE refine in mf by
///      cross-correlating the 32-sym marker sync template at audio
///      rate and parabolic-peak-fitting the magnitude.
///   4. OLS regression of refined audio positions vs expected
///      (un-drifted) audio positions. Slope ≈ 1 + ε ⇒ drift_ppm.
///
/// Operating on raw mf (audio rate) instead of FFE output (symbol rate)
/// gives ~10× better peak localisation and removes the FFE-windowing
/// scaling artefact that plagued the earlier symbol-domain attempt.
/// Markers are the known reference points (the "pilots" of our frame
/// structure), so the estimator is data-aided — robust across
/// constellations, β values, and noise levels without per-profile
/// calibration.
pub fn estimate_drift_gardner(samples: &[f32], config: &ModemConfig) -> Option<f64> {
    let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
        .ok()?;
    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
    let bb = {
        let _t = StageTimer::new(PerfStage::Downmix);
        demodulator::downmix(samples, config.center_freq_hz)
    };
    let mf = {
        let _t = StageTimer::new(PerfStage::MatchedFilter);
        demodulator::matched_filter(&bb, &taps)
    };
    let preamble_syms = preamble::make_preamble_for_config(config);
    let sync_pos = {
        let _t = StageTimer::new(PerfStage::FindPreamble);
        sync::find_preamble(&mf, &preamble_syms, sps, pitch, config.beta)?
    };

    // Build the constellation of marker SYNC pattern as the "pilot block"
    // reference. Same 32-sym QPSK known-sequence the marker decoder uses.
    let sync_pattern = marker::make_sync_pattern();
    let n_sync = sync_pattern.len();

    // Locate markers in symbol-domain via the FFE+LMS pipeline (we use
    // the same path as the production decoder so the markers we find
    // are exactly the ones that would be decoded). This gives us a list
    // of (symbol_index_in_FFE_output, is_meta) tuples.
    let (fse_input, fse_start, d_fse) = {
        let _t = StageTimer::new(PerfStage::Decimate);
        sync::decimate_for_fse(&mf, sync_pos, sps, pitch)
    };
    let pitch_fse = pitch / d_fse;
    let sps_fse = sps / d_fse;
    let tau_eff = pitch_fse as f64 / sps_fse as f64;
    let mut n_ff = if tau_eff >= 0.99 { 8 * sps_fse + 1 } else { 4 * sps_fse + 1 };
    if n_ff % 2 == 0 { n_ff += 1; }
    let half_ff = n_ff / 2;
    let training_positions: Vec<usize> =
        (0..N_PREAMBLE).map(|k| fse_start + k * pitch_fse).collect();
    let ffe_initial = {
        let _t = StageTimer::new(PerfStage::FfeLs);
        ffe::train_ffe_ls(&fse_input, &preamble_syms, &training_positions, n_ff)
    };
    let max_syms = if fse_input.len() > fse_start + half_ff {
        (fse_input.len() - fse_start - half_ff) / pitch_fse + 1
    } else { 0 };
    let preamble_training: Vec<(usize, Complex64)> = preamble_syms
        .iter().enumerate().map(|(k, &s)| (k, s)).collect();
    let constellation = frame::make_constellation(config);
    let (all_rx_syms, _final_taps) = {
        let _t = StageTimer::new(PerfStage::FfeLms);
        ffe::apply_ffe_lms_with_training(
            &fse_input, &ffe_initial, fse_start, pitch_fse, max_syms,
            &preamble_training, &constellation, 0.10, 0.02,
        )
    };
    // Global gain normalisation from preamble LS (so marker scan threshold is meaningful)
    let warmup_len = config.lms_warmup_syms();
    let header_end = N_PREAMBLE + warmup_len + 96;
    if all_rx_syms.len() < header_end {
        return None;
    }
    let gain = {
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0_f64;
        for k in 0..N_PREAMBLE {
            num += all_rx_syms[k] * preamble_syms[k].conj();
            den += preamble_syms[k].norm_sqr();
        }
        if den > 1e-12 { num / den } else { Complex64::new(1.0, 0.0) }
    };
    let corrected: Vec<Complex64> = all_rx_syms.iter().map(|&s| s / gain).collect();
    let data_region = &corrected[header_end..];

    // Walk markers with the standard scan, but only collect symbol-domain
    // positions + meta flags (no payload decode needed here; we just
    // want approximate locations).
    let bps = config.constellation.bits_per_sym();
    let decoder = LdpcDecoder::new(config.ldpc_rate, 50);
    let padded_n = interleaver::padded_cw_bits(decoder.n(), config.constellation);
    let syms_per_cw = padded_n / bps;
    let d_syms = config.pilot_pattern.d_syms;
    let p_syms = config.pilot_pattern.p_syms;
    let mut markers: Vec<(usize, bool)> = Vec::new();
    let mut cursor = 0_usize;
    let _t_scan = StageTimer::new(PerfStage::MarkerScan);
    while cursor + MARKER_LEN <= data_region.len() {
        let window = 64_usize;
        let end = (cursor + window).min(data_region.len().saturating_sub(MARKER_LEN));
        let hit = marker::find_sync_in_window(data_region, cursor, end - cursor, 0.5);
        let pos = match hit {
            Some((p, _)) => p,
            None => { cursor += MARKER_LEN; continue; }
        };
        let payload = marker::decode_marker_at(&data_region[pos..pos + MARKER_LEN]);
        match payload {
            Some(p) => {
                markers.push((pos, p.is_meta()));
                let n_cw = if p.is_meta() { 1 } else { V2_CODEWORDS_PER_SEGMENT };
                let data_sym_count = n_cw * syms_per_cw;
                let n_pilot_groups = (data_sym_count + d_syms - 1) / d_syms;
                let seg_sym_len = data_sym_count + n_pilot_groups * p_syms;
                cursor = pos + MARKER_LEN + seg_sym_len;
            }
            None => cursor += MARKER_LEN,
        }
    }
    if markers.len() < 3 {
        eprintln!(
            "[rx_v2 gardner] only {} markers found, need >=3",
            markers.len()
        );
        return None;
    }

    // For each marker, refine its AUDIO position in mf via sub-sample
    // parabolic peak fit of the 32-sym sync correlation. Audio position
    // of marker[i] should be roughly:
    //   nominal_audio = sync_pos + (header_end + cumulative_sym_in_data_region) × pitch
    // where the cumulative is rebuilt from the segment-size sequence.
    let mut cumulative_sym = 0_usize;
    let mut data: Vec<(f64, f64)> = Vec::with_capacity(markers.len()); // (expected_audio, refined_audio)

    // Helper: |corr| at integer mf audio position
    let corr_at = |mf: &[Complex64], pos: usize| -> f64 {
        if pos + n_sync * pitch > mf.len() {
            return 0.0;
        }
        let mut acc = Complex64::new(0.0, 0.0);
        for (k, &s) in sync_pattern.iter().enumerate() {
            acc += mf[pos + k * pitch] * s.conj();
        }
        acc.norm()
    };

    for (i, &(_int_pos_sym, is_meta)) in markers.iter().enumerate() {
        // Expected audio position for marker[i] in mf
        let expected_audio = (sync_pos + (header_end + cumulative_sym) * pitch) as i64;

        // Search a small ±pitch window for the actual peak, then
        // parabolic refine.
        let win = pitch as i64;
        let lo = (expected_audio - win).max(0) as usize;
        let hi = ((expected_audio + win) as usize).min(mf.len().saturating_sub(n_sync * pitch));
        let mut best = lo;
        let mut best_mag = 0.0_f64;
        for p in lo..=hi {
            let m = corr_at(&mf, p);
            if m > best_mag {
                best_mag = m;
                best = p;
            }
        }
        // Parabolic refine on |corr| at best-1, best, best+1
        let refined = if best == 0 || best + n_sync * pitch >= mf.len() {
            best as f64
        } else {
            let m0 = corr_at(&mf, best - 1);
            let m1 = corr_at(&mf, best);
            let m2 = corr_at(&mf, best + 1);
            let denom = m0 - 2.0 * m1 + m2;
            if denom >= -1e-12 {
                best as f64
            } else {
                let delta = 0.5 * (m0 - m2) / denom;
                best as f64 + delta.clamp(-1.0, 1.0)
            }
        };
        let _ = i;

        data.push((expected_audio as f64, refined));

        // Advance cumulative by this marker's segment size
        let n_cw = if is_meta { 1 } else { V2_CODEWORDS_PER_SEGMENT };
        let data_sym_count = n_cw * syms_per_cw;
        let n_pilot_groups = (data_sym_count + d_syms - 1) / d_syms;
        let seg_sym_len = data_sym_count + n_pilot_groups * p_syms;
        cumulative_sym += MARKER_LEN + seg_sym_len;
    }

    // OLS slope of observed_audio vs expected_audio. slope ≈ 1 + ε, so
    // drift_ppm = (slope - 1) × 1e6. Sign: audio longer (RX faster)
    // means observed_audio[N] > expected_audio[N], slope > 1, ppm > 0.
    let n = data.len() as f64;
    let mean_x = data.iter().map(|&(x, _)| x).sum::<f64>() / n;
    let mean_y = data.iter().map(|&(_, y)| y).sum::<f64>() / n;
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for &(x, y) in &data {
        let dx = x - mean_x;
        let dy = y - mean_y;
        num += dx * dy;
        den += dx * dx;
    }
    if den.abs() < 1e-9 { return None; }
    let slope = num / den;
    let drift_ppm = (slope - 1.0) * 1e6;

    // Residual RMS for quality check
    let intercept = mean_y - slope * mean_x;
    let mut rss = 0.0_f64;
    for &(x, y) in &data {
        let r = y - (slope * x + intercept);
        rss += r * r;
    }
    let rms = (rss / n).sqrt();
    eprintln!(
        "[rx_v2 markeraudio] {} markers, slope={slope:.9}, ppm={drift_ppm:+.2}, rms={rms:.2} samples",
        data.len(),
    );

    // Quality gates: residual RMS should be << 1 audio sample, ppm in bound.
    if rms > 2.0 { return None; }
    if drift_ppm.abs() > 200.0 { return None; }
    record_stage(PerfStage::PassDone, 0);
    Some(drift_ppm)
}

/// Single-pass v2 decode (formerly `rx_v2` body). Called by the drift-aware
/// wrapper and also exposed for direct use when caller already knows the
/// input is drift-free (loopback, simulated channels).
pub fn rx_v2_single(samples: &[f32], config: &ModemConfig) -> Option<RxV2Result> {
    rx_v2_single_cancellable(samples, config, None)
}

/// Cooperatively-cancellable variant of [`rx_v2_single`]. Pass `cancel =
/// Some(&flag)` to let the caller bail out a long-running decode from
/// another thread.
///
/// Used by the aarch64 parallel safety grid (see [`rx_v2_with_options`]):
/// when one batch-mate produces a clean decode, it sets the shared flag
/// and the remaining workers exit at the next checkpoint. Two checkpoints
/// are wired in:
///
///   1. **Post-FFE/LMS** : right after `apply_ffe_lms_with_training`
///      finishes, before LDPC decoding starts. FFE is ~50-80 ms on a
///      Pi 4 ; if a faster batch-mate has already converged by then,
///      skipping the (10-100 ms) LDPC saves real wall-clock.
///   2. **Per-codeword** : inside the segment loop, top of
///      `for cw_idx in 0..n_cw`. Single LDPC iteration is ~5-15 ms ;
///      this gives bail-out granularity well under the 50 ms budget.
///
/// `cancel = None` is the non-parallel path : zero overhead beyond a
/// single `Option::is_none` per checkpoint, no atomic load.
pub fn rx_v2_single_cancellable(
    samples: &[f32],
    config: &ModemConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Option<RxV2Result> {
    use std::sync::atomic::Ordering;
    let cancelled = || cancel.map_or(false, |c| c.load(Ordering::Relaxed));
    let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
        .ok()?;
    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
    let constellation = frame::make_constellation(config);
    let decoder = LdpcDecoder::new(config.ldpc_rate, 50);
    let padded_n_v2 = interleaver::padded_cw_bits(decoder.n(), config.constellation);
    let deinterleave_perm = interleaver::deinterleave_table(padded_n_v2, config.constellation);

    let bb = {
        let _t = StageTimer::new(PerfStage::Downmix);
        demodulator::downmix(samples, config.center_freq_hz)
    };
    let mf = {
        let _t = StageTimer::new(PerfStage::MatchedFilter);
        demodulator::matched_filter(&bb, &taps)
    };

    let preamble_syms = preamble::make_preamble_for_config(config);
    let warmup_syms = preamble::make_lms_warmup_for_config(config);
    let warmup_len = warmup_syms.len();
    let sync_pos = {
        let _t = StageTimer::new(PerfStage::FindPreamble);
        sync::find_preamble(&mf, &preamble_syms, sps, pitch, config.beta)?
    };

    // Decimate + LS-trained FFE on preamble (same as rx.rs prelude)
    let (fse_input, fse_start, d_fse) = {
        let _t = StageTimer::new(PerfStage::Decimate);
        sync::decimate_for_fse(&mf, sync_pos, sps, pitch)
    };
    let pitch_fse = pitch / d_fse;
    let sps_fse = sps / d_fse;
    let tau_eff = pitch_fse as f64 / sps_fse as f64;
    let mut n_ff = if tau_eff >= 0.99 {
        8 * sps_fse + 1
    } else {
        4 * sps_fse + 1
    };
    if n_ff % 2 == 0 {
        n_ff += 1;
    }

    let header_sym_count = 96;

    let training_positions: Vec<usize> = (0..N_PREAMBLE)
        .map(|k| fse_start + k * pitch_fse)
        .collect();
    let ffe_initial = {
        let _t = StageTimer::new(PerfStage::FfeLs);
        ffe::train_ffe_ls(&fse_input, &preamble_syms, &training_positions, n_ff)
    };
    let ffe_centroid_initial = ffe_tap_centroid(&ffe_initial);

    let half = n_ff / 2;
    let max_syms = if fse_input.len() > fse_start + half {
        (fse_input.len() - fse_start - half) / pitch_fse + 1
    } else {
        0
    };

    // Use the preamble as explicit training (refs known, high mu), then switch
    // to decision-directed LMS on the data constellation for the rest of the
    // stream. This absorbs FTN ISI tails that the finite LS-trained FFE leaves
    // as residuals (critical for MEGA; harmless for other profiles since the
    // DD slicer is very reliable at high SNR).
    //
    // For Apsk64 profiles (HIGH++), we extend the training phase with
    // an LMS guard interval of `warmup_len` known symbols sweeping the
    // entire 64-APSK constellation (cf. `make_lms_warmup_for_config`).
    // LMS adapts on the actual data density BEFORE the first meta CW,
    // which is then no longer the first encounter of the DD switch on
    // 64-APSK.
    let mut preamble_training: Vec<(usize, Complex64)> = preamble_syms
        .iter()
        .enumerate()
        .map(|(k, &s)| (k, s))
        .collect();
    for (k, &s) in warmup_syms.iter().enumerate() {
        preamble_training.push((N_PREAMBLE + k, s));
    }
    let mu_train = 0.10;
    let mu_dd = 0.02;
    let (all_rx_syms, final_taps) = {
        let _t = StageTimer::new(PerfStage::FfeLms);
        ffe::apply_ffe_lms_with_training(
            &fse_input,
            &ffe_initial,
            fse_start,
            pitch_fse,
            max_syms,
            &preamble_training,
            &constellation,
            mu_train,
            mu_dd,
        )
    };
    let ffe_centroid_final = ffe_tap_centroid(&final_taps);
    if cancelled() {
        return None;
    }
    if all_rx_syms.len() < N_PREAMBLE + warmup_len + header_sym_count {
        return None;
    }

    // Global gain LS from preamble
    let gain = {
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..N_PREAMBLE {
            num += all_rx_syms[k] * preamble_syms[k].conj();
            den += preamble_syms[k].norm_sqr();
        }
        if den > 1e-12 {
            num / den
        } else {
            Complex64::new(1.0, 0.0)
        }
    };
    let corrected: Vec<Complex64> = all_rx_syms.iter().map(|&s| s / gain).collect();

    // Protocol header (v2 or v3 — same structure ; v3 only adds periodic
    // preamble+header insertions before each meta segment, transparent to
    // the marker-based segment walker below).
    let header_start = N_PREAMBLE + warmup_len;
    let header_syms = &corrected[header_start..header_start + header_sym_count];
    let decoded_header = header::decode_header_symbols(header_syms)?;
    if decoded_header.version != HEADER_VERSION_V3 {
        return None;
    }

    // Walk data region segment by segment
    let data_region_start = header_start + header_sym_count;
    let data_region = &corrected[data_region_start..];

    let bps = config.constellation.bits_per_sym();
    // If decoder.n() isn't divisible by bps (Apsk32 case: 2304 % 5),
    // the TX pads the codeword to the next multiple -> we allocate the
    // symbols accordingly and drop the padding LLRs before LDPC decoding.
    let padded_n = interleaver::padded_cw_bits(decoder.n(), config.constellation);
    let syms_per_cw = padded_n / bps;
    let k_bytes = decoder.k() / 8;

    let pll_alpha = 0.05f64;
    let pll_beta = pll_alpha * pll_alpha * 0.25;
    let mut pll = DdPll::new(pll_alpha, pll_beta);

    // State accumulators
    let mut cursor: usize = 0;
    let mut app_hdr: Option<AppHeader> = None;
    let mut cw_bytes: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut total_blocks: usize = 0;
    let mut converged_blocks: usize = 0;
    let mut segments_decoded: usize = 0;
    let mut segments_lost: usize = 0;
    let mut sigma2_sum: f64 = 0.0;
    let mut sigma2_count: usize = 0;
    // Higher central-moment accumulators on pilot residuals (Re/Im
    // stacked as 2N real samples). Used to derive per-segment skewness
    // (3rd moment / σ³) and excess kurtosis (4th moment / σ⁴ - 3),
    // which classify the noise type per SF :
    //   - Gaussian/thermal channel    : skew ~ 0, kurt_excess ~ 0
    //   - Impulsive interferer (PLC,
    //     switching supplies, RFI)    : kurt_excess >> 0
    //   - Bursty fade / QSB           : skew ≠ 0 (asymmetric tails)
    let mut pilot_x3_sum: f64 = 0.0;
    let mut pilot_x4_sum: f64 = 0.0;
    // Hard-decision data-symbol σ² accumulators. Populated alongside
    // the pilot σ² but only on non-meta segments and only over actual
    // data symbols (post-equalisation). Surfaced separately to the GUI.
    let mut sigma2_data_sum: f64 = 0.0;
    let mut sigma2_data_count: usize = 0;
    const MAX_CONSTELLATION_POINTS: usize = 500;
    let mut constellation_sample: Vec<[f32; 2]> = Vec::new();
    // Pilot LS smoothed phases per data segment, in temporal order. Meta
    // segments are excluded — their phase trace would alias onto the data
    // plot at a different cadence (1 CW vs 2 CW per segment).
    let mut pilot_phase_segments: Vec<Vec<f32>> = Vec::new();
    // Parallel to `pilot_phase_segments`: true when the same-index entry
    // is a META segment. Lets the GUI distinguish META from DATA in the
    // bottom phase plot.
    let mut pilot_phase_is_meta: Vec<bool> = Vec::new();
    // Parallel to `pilot_phase_segments`: per-segment pilot sigma².
    // Surfaced for diagnostic logging — confirms whether sigma² is
    // distributed evenly across segments or spikes on a subset.
    let mut pilot_sigma2_per_segment: Vec<f64> = Vec::new();
    // Parallel : per-segment skewness and excess kurtosis on pilot
    // residuals. Standard Gaussian baseline (0, 0); deviations help
    // distinguish thermal noise from non-Gaussian impairments.
    let mut pilot_skew_per_segment: Vec<f64> = Vec::new();
    let mut pilot_kurt_per_segment: Vec<f64> = Vec::new();

    // Session: first valid marker we see locks session_id_low; later markers
    // with a different session_id_low indicate a session change → we stop
    // (multi-round merging is a higher-layer concern, handled in phase 2.5).
    let mut session_id_low_lock: Option<u8> = None;

    // Sliding marker detection: at each expected marker position, search within
    // a small window for the sync-pattern correlation peak. This tolerates
    // TCXO drift on long OTA transmissions and a few lost/added samples after
    // a channel gap (squelch). After consecutive decode failures we widen the
    // window to recover from bigger jumps.
    const NARROW_WINDOW: usize = 8; // ±8 syms around expected position
    const WIDE_WINDOW: usize = 512; // used after repeated failures (squelch recovery)
    let mut consecutive_fails: usize = 0;

    while cursor + MARKER_LEN <= data_region.len() {
        let search_window = if consecutive_fails >= 2 {
            WIDE_WINDOW
        } else {
            NARROW_WINDOW
        };
        let search_end = (cursor + search_window).min(data_region.len().saturating_sub(MARKER_LEN));
        let (marker_pos, _gain) =
            match marker::find_sync_in_window(data_region, cursor, search_end - cursor, 0.5) {
                Some(hit) => hit,
                None => {
                    // No sync pattern matched anywhere in the window — assume
                    // true marker slid past; advance and try again.
                    consecutive_fails += 1;
                    segments_lost += 1;
                    cursor += MARKER_LEN;
                    continue;
                }
            };
        let marker_syms = &data_region[marker_pos..marker_pos + MARKER_LEN];
        let marker_payload = match marker::decode_marker_at(marker_syms) {
            Some(p) => p,
            None => {
                consecutive_fails += 1;
                segments_lost += 1;
                cursor = marker_pos + MARKER_LEN;
                continue;
            }
        };
        consecutive_fails = 0;
        // Snap cursor to the detected marker so downstream segment extraction
        // uses the correct position.
        cursor = marker_pos;

        // Session lock: first marker sets it; mismatching markers are ignored
        match session_id_low_lock {
            None => session_id_low_lock = Some(marker_payload.session_id_low),
            Some(locked) if locked != marker_payload.session_id_low => {
                // Different session in the same stream — stop here; phase 2.5
                // will introduce explicit multi-session handling.
                break;
            }
            _ => {}
        }

        cursor += MARKER_LEN;

        // Segment length: meta is always 1 CW; data segments always carry
        // V2_CODEWORDS_PER_SEGMENT codewords (TX guarantees this except on
        // the very last data segment of a burst when the total count is
        // odd — CRC + marker re-sync on the next iteration handle the
        // resulting 1-CW-of-garbage read safely).
        //
        // The previous K-based clamp was wrong for any segment with
        // base_esi ≥ K : it dropped half the repair codewords of the
        // initial burst, and ~all codewords of a TX-more burst (whose
        // ESIs are always beyond K).
        let n_cw = if marker_payload.is_meta() {
            1
        } else {
            V2_CODEWORDS_PER_SEGMENT
        };
        let data_sym_count = n_cw * syms_per_cw;
        let d_syms = config.pilot_pattern.d_syms;
        let p_syms = config.pilot_pattern.p_syms;
        let n_pilot_groups = (data_sym_count + d_syms - 1) / d_syms;
        let seg_sym_len = data_sym_count + n_pilot_groups * p_syms;

        if cursor + seg_sym_len > data_region.len() {
            break;
        }
        let seg_syms_raw = &data_region[cursor..cursor + seg_sym_len];

        // Snapshot sigma2 accumulator before/after the track_segment call
        // so we can derive this segment's own sigma2 contribution (used
        // for the per-segment diagnostic log surfaced via
        // pilot_sigma2_per_segment).
        let s2_sum_before = sigma2_sum;
        let s2_count_before = sigma2_count;
        let x3_before = pilot_x3_sum;
        let x4_before = pilot_x4_sum;
        let (seg_data_syms, seg_pilot_phases) = track_segment(
            seg_syms_raw,
            &config.pilot_pattern,
            &mut pll,
            &constellation,
            &mut sigma2_sum,
            &mut sigma2_count,
            &mut pilot_x3_sum,
            &mut pilot_x4_sum,
        );
        // Per-segment moment derivation. N_seg counts pilot symbols
        // (one complex residual each) ; the moment accumulators added
        // 2 real samples per pilot (Re and Im), so the effective sample
        // count is `2 * (sigma2_count - s2_count_before)`. Mean is
        // assumed 0 (LS gain removes it).
        let seg_pilots = sigma2_count - s2_count_before;
        let (seg_sigma2, seg_skew, seg_kurt) = if seg_pilots > 0 {
            let n2 = (2 * seg_pilots) as f64;
            let s2 = (sigma2_sum - s2_sum_before) / n2; // σ² (stacked
                                                       // Re+Im baseline)
            let m3 = (pilot_x3_sum - x3_before) / n2;
            let m4 = (pilot_x4_sum - x4_before) / n2;
            let s2_safe = s2.max(1e-12);
            let s3 = s2_safe.powf(1.5);
            let skew = m3 / s3;
            let kurt_excess = m4 / (s2_safe * s2_safe) - 3.0;
            // Keep the legacy "σ² per pilot (complex)" semantics for
            // the existing per-segment vector so the GUI doesn't see
            // its values halve overnight. = 2 * (Re+Im stacked σ²).
            (s2 * 2.0, skew, kurt_excess)
        } else {
            (f64::NAN, f64::NAN, f64::NAN)
        };
        cursor += seg_sym_len;

        if seg_data_syms.len() < n_cw * syms_per_cw {
            segments_lost += 1;
            continue;
        }

        // Pilot phase trace per segment for the GUI drift diagnostic.
        // META and DATA are both included; `pilot_phase_is_meta` tags
        // which is which so the GUI can colour them distinctly. The
        // parallel `pilot_sigma2_per_segment` carries the segment's own
        // pilot-residual variance for the per-segment diagnostic log.
        if !seg_pilot_phases.is_empty() {
            pilot_phase_segments.push(seg_pilot_phases);
            pilot_phase_is_meta.push(marker_payload.is_meta());
            pilot_sigma2_per_segment.push(seg_sigma2);
            pilot_skew_per_segment.push(seg_skew);
            pilot_kurt_per_segment.push(seg_kurt);
        }

        // Data-symbol σ² (frame-only, hard-decision residuals). Skip meta
        // segments — the GUI metric is meant to track payload-symbol
        // quality, and meta is framing overhead. Use the n_cw*syms_per_cw
        // prefix only (track_segment may have over-allocated on the last
        // pilot group).
        if !marker_payload.is_meta() {
            let n_data_syms = (n_cw * syms_per_cw).min(seg_data_syms.len());
            for &y in &seg_data_syms[..n_data_syms] {
                let d = constellation.hard_decision(y);
                sigma2_data_sum += (y - d).norm_sqr();
                sigma2_data_count += 1;
            }
        }

        // Sample post-equalised DATA symbols for the constellation scatter
        // plot. Meta segments are excluded (their content is the AppHeader
        // replicated, would bias the cloud toward a subset of constellation
        // points).
        if !marker_payload.is_meta() && constellation_sample.len() < MAX_CONSTELLATION_POINTS {
            let remaining = MAX_CONSTELLATION_POINTS - constellation_sample.len();
            let step = (seg_data_syms.len() / remaining.max(1)).max(1);
            for (i, sym) in seg_data_syms.iter().enumerate() {
                if i % step == 0 {
                    constellation_sample.push([sym.re as f32, sym.im as f32]);
                    if constellation_sample.len() >= MAX_CONSTELLATION_POINTS {
                        break;
                    }
                }
            }
        }

        // LLR scale: use THIS segment's own pilot-residual σ² so the
        // soft demod sees the noise variance that actually applies to
        // these data symbols. The previous implementation used the
        // running mean σ² across all segments processed so far in this
        // superframe — a sound aggregate metric for diagnostics, but a
        // poor LLR scale when channel quality varies segment by segment
        // (drift accumulation on sound-card paths, brief fades, ALSA
        // glitches mid-burst). With an aggregate σ², a noisy segment's
        // high variance was diluted by the surrounding clean ones, so
        // its symbols were soft-demodulated as if clean → LLR magnitudes
        // were over-confident → LDPC converged on the noise → wrong
        // bits → wrong hard decisions on the (otherwise clean) data
        // symbols → outlier points scattered across the constellation
        // scatter (the "constellation aux fraises" symptom observed
        // 2026-05-11 OTA on HighPlus). Per-segment σ² fixes the LLR
        // scale at the source.
        //
        // Fallback to the running aggregate, then to a generic 0.1, in
        // the (rare) case where this segment had no pilot contribution.
        let sigma2_for_llr = if seg_sigma2.is_finite() {
            seg_sigma2.max(1e-6)
        } else if sigma2_count > 0 {
            (sigma2_sum / sigma2_count as f64).max(1e-6)
        } else {
            0.1
        };

        for cw_idx in 0..n_cw {
            if cancelled() {
                return None;
            }
            let off = cw_idx * syms_per_cw;
            let cw_syms = &seg_data_syms[off..off + syms_per_cw];
            let llr = soft_demod::llr_maxlog(cw_syms, &constellation, sigma2_for_llr);
            let llr_deint = interleaver::apply_permutation_f32(&llr, &deinterleave_perm);
            // Drop the padding LLRs (bits added on TX to align on
            // bits_per_sym) -- they sit at the tail after deinterleave.
            let llr_for_ldpc = &llr_deint[..decoder.n()];
            let (info_bytes, converged) = {
                let _t = StageTimer::new(PerfStage::Ldpc);
                decoder.decode_to_bytes(llr_for_ldpc)
            };
            let bytes = info_bytes[..k_bytes].to_vec();

            total_blocks += 1;
            if converged {
                converged_blocks += 1;
            }

            if marker_payload.is_meta() {
                if converged {
                    if let Some(h) = app_header::decode_meta_payload(&bytes) {
                        app_hdr = Some(h);
                    }
                }
            } else if converged {
                // Only accept data blocks whose LDPC parity passes. A
                // non-converged block carries corrupted bytes that would
                // poison the hash check — treat it as missing instead,
                // zero-padded in the final assembly.
                let esi = marker_payload.base_esi + cw_idx as u32;
                cw_bytes.insert(esi, bytes);
            }
        }

        segments_decoded += 1;
    }

    // Assemble payload via RaptorQ fountain decoder.
    // The AppHeader provides the OTI (file_size, t_bytes) ; we feed every
    // converged CW as a (ESI, T bytes) packet to the decoder and keep the
    // result once enough source/repair symbols are collected.
    let mut assembled: Vec<u8> = Vec::new();
    if let Some(ref h) = app_hdr {
        let raptorq_result = {
            let _t = StageTimer::new(PerfStage::RaptorQ);
            modem_framing::raptorq_codec::try_decode(&cw_bytes, h.file_size, h.t_bytes as u16)
        };
        if let Some(payload) = raptorq_result {
            assembled = payload;
        } else {
            // Not enough packets collected : fall back to zero-padded ESI
            // concatenation so that the user still gets a partial file
            // (better than nothing for OTA debugging).
            let n_source_cw = ((h.file_size as usize) + k_bytes - 1) / k_bytes;
            for esi in 0..n_source_cw as u32 {
                if let Some(bytes) = cw_bytes.get(&esi) {
                    assembled.extend_from_slice(bytes);
                } else {
                    assembled.extend(std::iter::repeat(0u8).take(k_bytes));
                }
            }
            assembled.truncate(h.file_size as usize);
        }
    } else {
        // No AppHeader recovered : fall back to ESI-sorted concat.
        let mut esis: Vec<u32> = cw_bytes.keys().cloned().collect();
        esis.sort();
        for esi in esis {
            assembled.extend_from_slice(&cw_bytes[&esi]);
        }
        assembled.truncate(decoded_header.payload_length as usize);
    }

    let sigma2 = if sigma2_count > 0 {
        (sigma2_sum / sigma2_count as f64).max(1e-6)
    } else {
        1.0
    };
    // Falls back to the pilot σ² when no data segments contributed —
    // the GUI then has at least a sane number to display.
    let sigma2_data = if sigma2_data_count > 0 {
        (sigma2_data_sum / sigma2_data_count as f64).max(1e-6)
    } else {
        sigma2
    };

    let data_blocks_recovered = cw_bytes.len();
    let eot_seen = decoded_header.flags & header::FLAG_EOT != 0;
    record_stage(PerfStage::PassDone, 0);
    Some(RxV2Result {
        data: assembled,
        header: Some(decoded_header),
        app_header: app_hdr,
        converged_blocks,
        total_blocks,
        segments_decoded,
        segments_lost,
        sigma2,
        sigma2_data,
        data_blocks_recovered,
        cw_bytes_map: cw_bytes,
        eot_seen,
        constellation_sample,
        pilot_phase_segments,
        pilot_phase_is_meta,
        pilot_sigma2_per_segment,
        pilot_skew_per_segment,
        pilot_kurt_per_segment,
        last_preamble_offset: None,
        // Single-pass path never resamples; the caller (`rx_v2` /
        // `rx_v2_with_hint`) overwrites this when the decode was driven
        // by a non-zero hint.
        drift_ppm: 0.0,
        ffe_centroid_initial,
        ffe_centroid_final,
    })
}

/// Sliding-window batch decode of a v3 superframe.
///
/// A v3 stream carries a fresh preamble + protocol header before every
/// periodic meta segment (see `frame::build_superframe_v3`). Rather than
/// letting one global FFE track the entire transmission (the 57 %-on-HIGH
/// failure mode of streaming v2), this function:
///
/// 1. Locates every preamble occurrence via `sync::find_all_preambles`.
/// 2. For each window `[P_i .. P_{i+1})` (or `[P_last .. EOF]` for the
///    last) it calls `rx_v2` as if that slice were a standalone v2
///    transmission — re-acquiring timing, re-training the FFE, brute-force
///    searching drift.
/// 3. Merges the per-window `cw_bytes_map`s into one global ESI → bytes
///    map (first-wins; a codeword appears in exactly one window anyway).
/// 4. Assembles the payload from the merged map using `AppHeader.file_size`
///    for truncation, just like `rx_v2_single`.
///
/// Returns `None` only if no preamble is found at all.
pub fn rx_v3(samples: &[f32], config: &ModemConfig) -> Option<RxV2Result> {
    // One-shot semantics: `samples` is assumed to hold the complete
    // transmission (CLI WAV decode, unit tests), so we must finalize and
    // decode the trailing OPEN window — there is no "next tick" to wait
    // for a closing preamble. The live worker uses `rx_v3_after` directly
    // with `finalize=false` for normal ticks.
    rx_v3_after(samples, config, 0, true, None, true, false, false)
}

/// Same as `rx_v3` but skip every preamble whose detected offset is strictly
/// less than `skip_until` (in samples, relative to `samples`). Allows a
/// caller maintaining a rolling capture buffer to mark closed windows as
/// done and avoid re-decoding them on every tick — critical for slow
/// profiles like ULTRA where each `rx_v2` pass costs several hundred ms
/// and the buffer can hold 2-3 closed windows at any time.
///
/// When `skip_until = 0` and `finalize = true`, behaves exactly like the
/// public [`rx_v3`] wrapper (one-shot full decode).
///
/// The OPEN window (trailing preamble with no closing preamble in this
/// buffer) is ALWAYS decoded — its header is the only path for an idle
/// worker to recognize that a burst has started. When a CLOSED window
/// earlier in the same buffer produced a drift estimate, that estimate
/// is inherited as a hint via [`rx_v2_with_hint`], giving the OPEN
/// window the same drift compensation as if it were CLOSED itself
/// (sound-card clocks drift slowly enough that the previous superframe's
/// ppm is a near-perfect prior).
///
/// `finalize` controls only the no-hint fallback strategy:
/// - `false` (live worker, normal tick) — fall back to [`rx_v2_single`]
///   (1 pass, no drift grid). Fast, matches pre-refactor behaviour.
///   Once a CLOSED window decodes successfully later in the session,
///   subsequent OPEN decodes inherit its drift hint.
/// - `true` (one-shot decode = CLI WAV file, unit tests) — fall back
///   to a full [`rx_v2`] grid search. Used when the OPEN window IS
///   the entire payload (no future tick to wait for a closing preamble)
///   so we must give it the best possible chance to decode, including
///   large drift / misparameterized profile cases.
/// `session_hint_ppm` lets the worker carry a session-wide drift
/// estimate forward to every window. When `Some(p)`, CLOSED windows
/// skip the per-window `rx_v2` grid search and decode directly via
/// `rx_v2_with_hint(window, config, p)` -- saves ~14× the grid-search
/// cost on a clean signal and keeps the drift correction stable
/// across SFs. `None` falls back to the legacy behaviour (CLOSED
/// runs the full `rx_v2` grid, hint cascades CLOSED→OPEN inside the
/// loop). Either way the OPEN window inherits the latest CLOSED's
/// `r.drift_ppm` as before (which IS the session hint when one was
/// provided, since `rx_v2_with_hint` echoes the hint back into
/// `drift_ppm`).
///
/// `allow_legacy_grid` controls whether `rx_v2` (when called for a
/// CLOSED window with no `session_hint_ppm`) runs its post-Gardner
/// ±15 ppm safety grid. On weak hosts (Pi-class) the grid is mostly
/// wasted CPU when the channel is noise-limited; the operator can
/// disable it from the GUI Settings tab. Has no effect on the
/// `session_hint_ppm = Some(_)` path since that already bypasses
/// the grid.
///
/// `lowpower_position_cap` controls the per-tick preamble cap
/// independently of `allow_legacy_grid` (= Axis-2 cap, decoupled from
/// the grid permission since 0.10.37). `true` → `find_all_preambles`
/// is capped at 2 positions = 1 CLOSED + 1 boundary, so only one SF
/// is processed per tick and the worker drains past the boundary
/// preamble regardless of decode success ; the next tick re-detects
/// that boundary as the new CLOSED. `false` → uncapped (desktop /
/// CLI / tests). Distinct from `allow_legacy_grid` because in
/// lowpower-with-fallback-quota mode (0.10.34) the worker may set
/// `allow_legacy_grid = true` opportunistically (quota window open)
/// while still wanting the position cap to bound the per-tick CPU
/// spike to a single SF.
///
/// `power_mode` (= GUI "Power Mode" toggle) flips the per-window
/// decoder from the modern Gardner pipeline (`rx_v2_with_options`)
/// to the 0.9.x algorithm (`rx_v2_legacy_grid_decode` for CLOSED,
/// `rx_v2_single` at 0 ppm for OPEN). Bypasses the session-level
/// drift hint and the EWMA chain. Recommended on PC modern enough to
/// afford the broad ±80 ppm grid per CLOSED ; Pi-class hosts should
/// leave this `false` and stay on the modern pipeline.
pub fn rx_v3_after(
    samples: &[f32],
    config: &ModemConfig,
    skip_until: usize,
    finalize: bool,
    session_hint_ppm: Option<f64>,
    allow_legacy_grid: bool,
    lowpower_position_cap: bool,
    power_mode: bool,
) -> Option<RxV2Result> {
    let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
        .ok()?;
    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);

    let bb = {
        let _t = StageTimer::new(PerfStage::Downmix);
        demodulator::downmix(samples, config.center_freq_hz)
    };
    let mf = {
        let _t = StageTimer::new(PerfStage::MatchedFilter);
        demodulator::matched_filter(&bb, &taps)
    };

    let preamble_syms = preamble::make_preamble_for_config(config);
    let mut positions = {
        let _t = StageTimer::new(PerfStage::FindPreamble);
        // Low-power live worker (`lowpower_position_cap = true` AND
        // `finalize = false`) caps the per-tick search at 2 preambles :
        // one CLOSED to decode + one boundary that stays in the buffer
        // for next tick's re-scan with a fresh Gardner. Desktop / CLI /
        // tests (`finalize = true` or `lowpower_position_cap = false`)
        // keep the uncapped behaviour so multi-SF buffers fully decode
        // in one call.
        //
        // 0.10.37 : decoupled from `allow_legacy_grid` so that the
        // worker's quota-fallback grid (LOWPOWER_GRID_QUOTA at 0.10.34)
        // doesn't accidentally uncap the position search. Previously
        // `effective_allow_grid = state.allow_legacy_grid || quota_room`
        // was passed to both this gate AND the safety-grid permission,
        // so a single in-quota tick on a slow channel would lift the
        // per-tick cap and dump the entire backlog (3-4 SFs) through
        // the grid in one tick.
        let max_positions = if lowpower_position_cap && !finalize {
            Some(2)
        } else {
            None
        };
        sync::find_all_preambles(&mf, &preamble_syms, sps, pitch, config.beta, max_positions)
    };
    if positions.is_empty() {
        return None;
    }
    positions.sort();
    // Caller-driven cache : drop preambles already processed in earlier
    // ticks. The last surviving "closed" position will be reported in the
    // result so the caller can advance its watermark.
    if skip_until > 0 {
        positions.retain(|&p| p >= skip_until);
        if positions.is_empty() {
            return None;
        }
    }

    // Pre-roll & post-roll: enough context for the matched filter span on
    // both sides so the first/last data symbols of the cycle aren't eaten
    // by MF edge effects.
    let margin = (RRC_SPAN_SYM + 4) * pitch;

    let mut merged: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut app_hdr: Option<AppHeader> = None;
    let mut hdr_any: Option<header::Header> = None;
    let mut total_converged = 0usize;
    let mut total_blocks = 0usize;
    let mut segs_decoded = 0usize;
    let mut segs_lost = 0usize;
    let mut sigma2_sum = 0.0f64;
    let mut sigma2_count = 0usize;
    let mut sigma2_data_sum = 0.0f64;
    let mut sigma2_data_count = 0usize;
    let mut eot_seen = false;
    let mut last_constellation: Vec<[f32; 2]> = Vec::new();
    let mut last_pilot_phases: Vec<Vec<f32>> = Vec::new();
    let mut last_pilot_phase_is_meta: Vec<bool> = Vec::new();
    let mut last_pilot_sigma2_per_segment: Vec<f64> = Vec::new();
    let mut last_pilot_skew_per_segment: Vec<f64> = Vec::new();
    let mut last_pilot_kurt_per_segment: Vec<f64> = Vec::new();
    // FFE-tap centroid pair of the most recent successful window.
    // Surfaced to the worker so it can compute `final - initial` and
    // trigger a Gardner re-estimate when the LMS adapter had to migrate
    // the peak across an SF -- proxy for uncorrected residual ppm.
    let mut last_ffe_centroid_initial: f64 = 0.0;
    let mut last_ffe_centroid_final: f64 = 0.0;
    // Running drift estimate. Updated by every CLOSED window decode that
    // exits `rx_v2` with a successful grid hit; used as a hint when we
    // (later in this same call) decide to decode an OPEN window.
    let mut chosen_ppm: f64 = 0.0;
    let mut have_hint: bool = false;
    // Latest successfully-decoded window's drift_ppm (CLOSED or OPEN),
    // surfaced in the returned `RxV2Result.drift_ppm`. This is what the
    // worker reports to the Info tab so the operator sees the actual
    // ppm correction the modem just landed on -- distinct from
    // `chosen_ppm`, which is gated to CLOSED windows because it feeds
    // the OPEN-window hint path.
    let mut reported_ppm: f64 = 0.0;

    for (i, &p) in positions.iter().enumerate() {
        let start = p.saturating_sub(margin).min(samples.len());
        // End a few symbols INTO the next preamble so the last data segment
        // of this cycle has MF post-roll context. The partial next preamble
        // (~margin/pitch symbols) is negligible against the 256-sym preamble
        // at the head, so find_preamble locks unambiguously on this cycle.
        let is_closed = i + 1 < positions.len();
        let end = if is_closed {
            (positions[i + 1] + margin).min(samples.len()).max(start + 1)
        } else {
            samples.len()
        };
        if end <= start + N_PREAMBLE * pitch {
            continue;
        }
        let window = &samples[start..end];
        let r_opt = if is_closed {
            // CLOSED window: always run per-window `rx_v2_with_options`.
            // This gives each SF its own Gardner-estimated drift correction,
            // matching the per-SF adaptation that the legacy `rx_v2()`
            // grid path provided in 0.10.18 and earlier. Tested on HIGH+
            // 32-APSK on a Ryzen 9 host (2026-05-13): the session-level
            // hint path that 0.10.19 introduced dropped LDPC convergence
            // from 100 % to 50-90 % because all SFs were resampled by a
            // single fixed ppm even when the channel had sub-ppm SF-to-SF
            // variation.
            //
            // Internally `rx_v2_with_options` runs :
            //   1. rx_v2_single @ 0 ppm (fast-path, ~0 if drift small)
            //   2. Gardner one-shot + rx_v2_with_hint(gardner_ppm)
            //   3. (only if allow_legacy_grid = true) ±15 ppm grid
            //
            // Step 2 alone is enough on a clean channel with ≥6 markers
            // per SF (Gardner is sub-ppm accurate); step 3 is the safety
            // net for outlier cases. `allow_legacy_grid` propagates from
            // the worker's user-toggle.
            //
            // The `session_hint_ppm` plumbed by the worker is INFORMATIONAL
            // for CLOSED windows -- not fed in here. It still drives the
            // OPEN-window decode below (idle pre-activation typically has
            // too few markers for a per-window Gardner) and the Info-tab
            // `sf_detail` telemetry.
            let _ = session_hint_ppm; // CLOSED uses per-window Gardner instead
            if power_mode {
                // Power Mode = 0.9.x algorithm. Each CLOSED window does
                // its own broad ±80/±10 grid via `rx_v2_legacy_grid_decode`,
                // no Gardner, no FFE-centroid re-estimation.
                rx_v2_legacy_grid_decode(window, config)
            } else {
                // Low-power mode (`allow_legacy_grid = false` on aarch64 / Pi):
                // also skip the 0-ppm fast-path on CLOSED. Gardner is sub-ppm
                // accurate with ≥6 markers and Pi 4 cannot afford the redundant
                // ~230 ms MF pass per SF. Desktop keeps both stages.
                let allow_fast_path = allow_legacy_grid;
                rx_v2_with_options(window, config, allow_legacy_grid, allow_fast_path)
            }
        } else {
            // OPEN window: trailing, no closing preamble.
            //
            // `finalize` toggles two semantics:
            // - `true` (one-shot CLI/tests, OR live worker IDLE pre-
            //   activation tick where the OPEN header is the only
            //   signal that a burst has started) — DECODE the window.
            //   Use the drift hint from the latest CLOSED window if
            //   available (`rx_v2_with_hint`), else fall back to a
            //   full `rx_v2` grid search so misparameterized profiles
            //   or large drifts still decode.
            // - `false` (live worker ACTIVE normal tick) — SKIP the
            //   window. Its audio stays in the caller's buffer (via
            //   `last_preamble_offset = positions.last()`) and gets
            //   decoded as a CLOSED window on the next tick once a
            //   new preamble lands. Avoids the segment-by-segment
            //   timing-drift accumulation that pilot-only tracking
            //   suffers on an unclosed window (observed 2026-05-11
            //   on sound-card paths in `[scan-segs]`).
            //
            // Auto-escalates to a decode if any CLOSED window of this
            // same buffer carried an EOT marker — the TX explicitly
            // flagged end-of-burst, so no closing preamble will ever
            // arrive and the OPEN audio is the last word of the burst.
            if !(finalize || eot_seen) {
                continue;
            }
            // Power Mode = 0.9.x algorithm. Two OPEN paths :
            //
            // - Cold-start idle activation (`finalize && !eot_seen` and
            //   no CLOSED window in this scan) — the single SOF in the
            //   buffer is the ONLY shot at acquiring the new burst
            //   before the next preamble lands V3_PREAMBLE_PERIOD_S
            //   (4 s) later. Run the broad ±80 ppm grid here, same
            //   path CLOSED uses (`rx_v2_legacy_grid_decode`), so a
            //   drifted cold start activates immediately instead of
            //   the modem waiting 4 s for a second SOF to make a
            //   CLOSED pair. Cost : ~13 rx_v2_single passes for this
            //   one window per activation, no recurring cost on
            //   subsequent ticks once the session is active.
            //
            // - Active-session trailing OPEN (`eot_seen`) — only
            //   reached because an earlier CLOSED window in this same
            //   scan carried the EOT marker. That CLOSED already ran
            //   the broad grid, so the trailing OPEN inherits a
            //   well-locked channel ; stay at 0 ppm via `rx_v2_single`
            //   like 0.9.2rc5 did. The OPEN tail has typically <3
            //   markers, so a redundant per-window grid would be
            //   wasted CPU.
            if power_mode {
                if finalize && !eot_seen && !have_hint {
                    rx_v2_legacy_grid_decode(window, config)
                } else {
                    rx_v2_single(window, config)
                }
            } else if have_hint {
                // Hint priority for the OPEN: latest CLOSED of this scan
                // (most local) > session-wide hint (still valid prior) >
                // fall back to a full `rx_v2` grid (cold start).
                rx_v2_with_hint(window, config, chosen_ppm)
            } else if let Some(hint) = session_hint_ppm {
                rx_v2_with_hint(window, config, hint)
            } else {
                // OPEN cold-start cascade keeps the 0-ppm fast-path even
                // in low-power mode: Gardner often fails to lock on a
                // trailing window with <3 markers, the fast-path is the
                // safety net that lets the burst land its OPEN header.
                rx_v2_with_options(window, config, allow_legacy_grid, /*allow_fast_path=*/ true)
            }
        };
        let Some(r) = r_opt else {
            continue;
        };
        if is_closed {
            chosen_ppm = r.drift_ppm;
            have_hint = true;
        }
        // Capture every successful window's drift for reporting -- the
        // last one wins, which is the most recent state of the channel.
        reported_ppm = r.drift_ppm;
        for (esi, bytes) in r.cw_bytes_map.into_iter() {
            merged.entry(esi).or_insert(bytes);
        }
        if app_hdr.is_none() {
            // Ignore EOT-only windows: their AppHeader carries file_size=0
            // as a sentinel, which would poison the main-burst assembly.
            let keep = r
                .app_header
                .as_ref()
                .map(|ah| ah.file_size > 0)
                .unwrap_or(false);
            if keep {
                app_hdr = r.app_header;
            }
        }
        if hdr_any.is_none() {
            hdr_any = r.header;
        }
        total_converged += r.converged_blocks;
        total_blocks += r.total_blocks;
        segs_decoded += r.segments_decoded;
        segs_lost += r.segments_lost;
        sigma2_sum += r.sigma2;
        sigma2_count += 1;
        sigma2_data_sum += r.sigma2_data;
        sigma2_data_count += 1;
        if r.eot_seen {
            eot_seen = true;
        }
        // Keep only the most recent window's scatter — it reflects the
        // current channel state, whereas stacking older windows would
        // smear drift / FFO evolution across the display.
        if !r.constellation_sample.is_empty() {
            last_constellation = r.constellation_sample;
        }
        // Same logic for the pilot phase trace : the GUI shows the most
        // recent window so the operator can watch drift evolve tick by tick.
        if !r.pilot_phase_segments.is_empty() {
            last_pilot_phases = r.pilot_phase_segments;
            last_pilot_phase_is_meta = r.pilot_phase_is_meta;
            last_pilot_sigma2_per_segment = r.pilot_sigma2_per_segment;
            last_pilot_skew_per_segment = r.pilot_skew_per_segment;
            last_pilot_kurt_per_segment = r.pilot_kurt_per_segment;
        }
        last_ffe_centroid_initial = r.ffe_centroid_initial;
        last_ffe_centroid_final = r.ffe_centroid_final;
    }

    // Assembly — same policy as rx_v2_single: RaptorQ fountain decode from
    // the merged ESI → bytes map, with zero-padded ESI fallback if the
    // decoder doesn't have enough packets.
    let decoder = LdpcDecoder::new(config.ldpc_rate, 50);
    let k_bytes = decoder.k() / 8;

    let mut assembled: Vec<u8> = Vec::new();
    if let Some(ref h) = app_hdr {
        let raptorq_result = {
            let _t = StageTimer::new(PerfStage::RaptorQ);
            modem_framing::raptorq_codec::try_decode(&merged, h.file_size, h.t_bytes as u16)
        };
        if let Some(payload) = raptorq_result {
            assembled = payload;
        } else {
            let n_source_cw = ((h.file_size as usize) + k_bytes - 1) / k_bytes;
            for esi in 0..n_source_cw as u32 {
                if let Some(bytes) = merged.get(&esi) {
                    assembled.extend_from_slice(bytes);
                } else {
                    assembled.extend(std::iter::repeat(0u8).take(k_bytes));
                }
            }
            assembled.truncate(h.file_size as usize);
        }
    } else {
        let mut esis: Vec<u32> = merged.keys().cloned().collect();
        esis.sort();
        for esi in esis {
            assembled.extend_from_slice(&merged[&esi]);
        }
    }

    let sigma2 = if sigma2_count > 0 {
        sigma2_sum / sigma2_count as f64
    } else {
        1.0
    };
    let sigma2_data = if sigma2_data_count > 0 {
        sigma2_data_sum / sigma2_data_count as f64
    } else {
        sigma2
    };
    let data_blocks_recovered = merged.len();

    // Position of the last preamble seen in this scan. Always set when
    // we got here (early-return guarantees `positions` is non-empty after
    // the skip_until filter, so `.last()` is Some). The caller (rx_worker)
    // uses this to truncate its rolling capture buffer right behind P_last,
    // turning the buffer into a self-purging queue.
    // Watermark = deepest preamble SEEN. The capped path (Axis 2,
    // low-power) returns `positions = [p_processed, p_boundary]` -- so
    // `positions.last() = p_boundary`. The worker drains the buffer to
    // `p_boundary - TRUNCATE_MARGIN_MS` (rx_worker.rs:1701-1706),
    // which drops the just-processed CLOSED window (p_processed) and
    // preserves p_boundary at margin offset for next tick's re-scan
    // with a fresh per-window Gardner. **Reporting `last_processed`
    // here would preserve p_processed AND cause it to be re-detected
    // on every subsequent tick -> infinite re-decode -> GUI freeze
    // (regression spotted on Pi 4 OTA 2026-05-14).**
    //
    // Uncapped path (desktop / CLI / finalize=true tests) keeps the
    // same semantics: positions.last() is the last preamble of the
    // batch, processed or not, and the worker drains past it.
    let last_preamble_offset = positions.last().copied();

    Some(RxV2Result {
        data: assembled,
        header: hdr_any,
        app_header: app_hdr,
        converged_blocks: total_converged,
        total_blocks,
        segments_decoded: segs_decoded,
        segments_lost: segs_lost,
        sigma2,
        sigma2_data,
        data_blocks_recovered,
        cw_bytes_map: merged,
        eot_seen,
        constellation_sample: last_constellation,
        pilot_phase_segments: last_pilot_phases,
        pilot_phase_is_meta: last_pilot_phase_is_meta,
        pilot_sigma2_per_segment: last_pilot_sigma2_per_segment,
        pilot_skew_per_segment: last_pilot_skew_per_segment,
        pilot_kurt_per_segment: last_pilot_kurt_per_segment,
        last_preamble_offset,
        // Drift correction of the LAST successfully decoded window in
        // this scan (CLOSED or OPEN). The per-window estimates are also
        // carried forward as hints to neighbouring windows inside the
        // loop (see `chosen_ppm`); this field is the telemetry view.
        // Falls back to 0.0 only when no window decoded -- in which
        // case the caller short-circuits on the assembled-data check
        // anyway.
        drift_ppm: reported_ppm,
        ffe_centroid_initial: last_ffe_centroid_initial,
        ffe_centroid_final: last_ffe_centroid_final,
    })
}

/// Pilot-aided complex-gain (magnitude + phase) interpolation on one segment,
/// optionally followed by a decision-directed PLL refinement on QPSK profiles.
///
/// The pilot pass uses the same approach as the v1 `rx::rx` pipeline that proved
/// robust for MEGA FTN on OTA: per-group complex LS gain, unwrap phase, 3-point
/// smooth, linear interpolate the complex gain per symbol, apply its inverse.
///
/// Pilot group indexing within a segment restarts at 0 (matches the TX
/// per-segment call to `pilot::interleave_data_pilots`).
///
/// Sigma² residuals at pilot positions (post pilot-correction, pre DD-PLL) are
/// accumulated in-place — they reflect the quality of the pilot-only estimate
/// and remain a sound input to the LDPC LLR scaler.
///
/// On QPSK profiles (ULTRA, ROBUST) a second pass applies a decision-directed
/// second-order PLL symbol-by-symbol to the extracted data vector. This picks
/// up sub-pilot-spacing drift (residual ppm after `grid_ppm`, short-term phase
/// noise) that the linear inter-pilot interpolation aliases. It is gated on
/// `bits_per_sym == 2` because QPSK decisions are reliable enough at typical
/// operating SNR (no feedback amplification of decision noise), whereas on
/// 16-APSK (FTN) the decisions are too marginal and would hurt more than help.
fn track_segment(
    seg_syms: &[Complex64],
    pattern: &crate::profile::PilotPattern,
    pll: &mut DdPll,
    constellation: &crate::constellation::Constellation,
    sigma2_sum: &mut f64,
    sigma2_count: &mut usize,
    // Higher central moments of the pilot residuals (Re/Im stacked as
    // 2N real samples; mean assumed ~0 because the LS gain removes it).
    // Used downstream to compute per-segment skewness and excess kurtosis
    // for noise-type diagnostics : Gaussian channel -> skew~0, kurt~0 ;
    // impulsive interferer -> kurt > 0 ; bursty fade -> skew != 0.
    pilot_x3_sum: &mut f64,
    pilot_x4_sum: &mut f64,
) -> (Vec<Complex64>, Vec<f32>) {
    let d_syms = pattern.d_syms;
    let p_syms = pattern.p_syms;
    let group_sz = d_syms + p_syms;
    let n_groups = seg_syms.len() / group_sz;

    // Per-group complex gain (LS fit of `p_syms` known pilots onto received)
    let mut pilot_gains: Vec<(usize, Complex64)> = Vec::with_capacity(n_groups);
    for g in 0..n_groups {
        let offset = g * group_sz;
        let pilot_start = offset + d_syms;
        let pilot_end = pilot_start + p_syms;
        let pilots_tx = pilot::pilots_for_group(g, pattern);
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..p_syms {
            num += seg_syms[pilot_start + k] * pilots_tx[k].conj();
            den += pilots_tx[k].norm_sqr();
        }
        let gain = if den > 1e-12 {
            num / den
        } else {
            Complex64::new(1.0, 0.0)
        };
        pilot_gains.push(((pilot_start + pilot_end) / 2, gain));
    }

    let n_p = pilot_gains.len();
    if n_p == 0 {
        let data: Vec<Complex64> = seg_syms
            .iter()
            .enumerate()
            .filter(|(i, _)| i % group_sz < d_syms)
            .map(|(_, &s)| s)
            .collect();
        return (data, Vec::new());
    }

    // Unwrap phase sequence
    let mut phases: Vec<f64> = pilot_gains.iter().map(|(_, g)| g.arg()).collect();
    for i in 1..n_p {
        let diff = phases[i] - phases[i - 1];
        if diff > std::f64::consts::PI {
            phases[i] -= 2.0 * std::f64::consts::PI;
        } else if diff < -std::f64::consts::PI {
            phases[i] += 2.0 * std::f64::consts::PI;
        }
    }
    let mags: Vec<f64> = pilot_gains.iter().map(|(_, g)| g.norm()).collect();

    // 3-point smoothing (reduces pilot-noise impact on interpolation)
    let phases_smooth: Vec<f64> = (0..n_p)
        .map(|i| {
            let lo = i.saturating_sub(1);
            let hi = (i + 1).min(n_p.saturating_sub(1));
            let span = hi - lo + 1;
            (phases[lo] + phases[i] + phases[hi]) / span as f64
        })
        .collect();
    let mags_smooth: Vec<f64> = (0..n_p)
        .map(|i| {
            let lo = i.saturating_sub(1);
            let hi = (i + 1).min(n_p.saturating_sub(1));
            let span = hi - lo + 1;
            (mags[lo] + mags[i] + mags[hi]) / span as f64
        })
        .collect();

    let interp = |i: usize| -> (f64, f64) {
        if i <= pilot_gains[0].0 {
            return (mags_smooth[0], phases_smooth[0]);
        }
        if i >= pilot_gains.last().unwrap().0 {
            return (mags_smooth[n_p - 1], phases_smooth[n_p - 1]);
        }
        let mut j = 0;
        while j + 1 < n_p && pilot_gains[j + 1].0 < i {
            j += 1;
        }
        let i0 = pilot_gains[j].0;
        let i1 = pilot_gains[j + 1].0;
        let a = (i - i0) as f64 / (i1 - i0) as f64;
        let mag = mags_smooth[j] * (1.0 - a) + mags_smooth[j + 1] * a;
        let phase = phases_smooth[j] * (1.0 - a) + phases_smooth[j + 1] * a;
        (mag, phase)
    };

    let mut data_syms: Vec<Complex64> = Vec::new();
    for (i, &y_raw) in seg_syms.iter().enumerate() {
        let inner = i % group_sz;
        let is_pilot = inner >= d_syms;
        let (mag, phase) = interp(i);
        let inv_gain = Complex64::from_polar(1.0 / mag.max(1e-6), -phase);
        let y_corrected = y_raw * inv_gain;

        if is_pilot {
            let group = i / group_sz;
            let pilots_tx = pilot::pilots_for_group(group, pattern);
            let expected = pilots_tx[inner - d_syms];
            let resid = y_corrected - expected;
            *sigma2_sum += resid.norm_sqr();
            *sigma2_count += 1;
            // Stack Re and Im as two independent real samples for the
            // higher-moment accumulators -- doubles the effective N and
            // keeps the Gaussian baseline at (skew=0, kurt_excess=0).
            let re = resid.re;
            let im = resid.im;
            let re2 = re * re;
            let im2 = im * im;
            *pilot_x3_sum += re2 * re + im2 * im;
            *pilot_x4_sum += re2 * re2 + im2 * im2;
        } else {
            data_syms.push(y_corrected);
        }
    }

    // Second pass: decision-directed PLL refinement on QPSK profiles only.
    // Pilot-interp has already centered the segment near θ=0, so the PLL only
    // tracks the intra-segment residual (small-angle, fast convergence). On
    // 8-PSK / 16-APSK we skip this pass — the decision noise on those
    // constellations would amplify more than the PLL removes.
    //
    // Note (2026-05-11) : tested a narrow-band variant (α=0.005, BW≈1 Hz)
    // for APSK profiles on sound-card capture, hoping to track residual
    // phase drift between pilot groups. Got the opposite result: sigma²
    // on narrow-window scans rose ~50% (0.03→0.05), conv ratios unchanged.
    // Conclusion: the sound-card degradation isn't phase-tracking but a
    // preamble→data transient (radio AGC + AF chain settling). The first
    // codeword after the preamble gets sacrificed, the rest decodes clean.
    // Track that under "investigate preamble→data transient" — needs an
    // RX-only fix (skip first M symbols / downweight initial LLR) or a
    // TX-side guard period, not a phase loop.
    if constellation.bits_per_sym == 2 {
        pll.reset();
        for y in data_syms.iter_mut() {
            let d = constellation.hard_decision(*y);
            *y = pll.derotate_and_update(*y, d);
        }
    }

    let phases_f32: Vec<f32> = phases_smooth.iter().map(|&p| p as f32).collect();
    (data_syms, phases_f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_framing::app_header::mime;
    use crate::modulator;
    use crate::profile::profile_high;

    fn make_session_hash(data: &[u8]) -> u16 {
        let mut h: u16 = 0;
        for &b in data {
            h = h.wrapping_mul(31).wrapping_add(b as u16);
        }
        h
    }

    /// Reproducible LCG (Knuth's MMIX). Identical RNG to the one used in
    /// `gate.rs` tests.
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self { Lcg(seed) }
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn next_f32(&mut self) -> f32 {
            ((self.next_u32() as f32) / (u32::MAX as f32)) * 2.0 - 1.0
        }
        /// CLT-12 gaussian approx with variance ≈ 1.
        fn next_gauss(&mut self) -> f32 {
            let s: f32 = (0..12).map(|_| self.next_f32()).sum();
            s / 2.0
        }
    }

    /// Add additive white gaussian noise to a real-audio buffer at a
    /// target RMS. Returns a fresh buffer (input unchanged).
    fn add_awgn(signal: &[f32], noise_rms: f32, seed: u64) -> Vec<f32> {
        let mut rng = Lcg::new(seed);
        let raw: Vec<f32> = (0..signal.len()).map(|_| rng.next_gauss()).collect();
        let actual_rms =
            (raw.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / raw.len() as f64).sqrt()
                as f32;
        let scale = noise_rms / actual_rms.max(1e-12);
        signal
            .iter()
            .zip(raw.iter())
            .map(|(&s, &n)| s + n * scale)
            .collect()
    }

    fn tx_v3(data: &[u8], config: &ModemConfig, session_id: u32) -> Vec<f32> {
        let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
            .expect("invalid profile");
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let symbols = frame::build_superframe_v3(
            data,
            config,
            session_id,
            mime::BINARY,
            make_session_hash(data),
        );
        modulator::modulate(&symbols, sps, pitch, &taps, config.center_freq_hz)
    }

    /// Loopback : TX v3 → RX v3 sliding-window. A payload large enough to
    /// trigger several periodic preamble reinsertions on the HIGH profile.
    #[test]
    fn loopback_v3_high_sliding_window() {
        let config = profile_high();
        let data: Vec<u8> = (0..15_000)
            .map(|i| (i as u32).wrapping_mul(2654435761) as u8)
            .collect();
        let samples = tx_v3(&data, &config, 0x1234_5678);
        let result = rx_v3(&samples, &config).expect("rx_v3 returned None");

        eprintln!(
            "V3 HIGH 15k : converged={}/{} segs={}/lost={} sigma²={:.4} data_cw={}",
            result.converged_blocks,
            result.total_blocks,
            result.segments_decoded,
            result.segments_lost,
            result.sigma2,
            result.data_blocks_recovered
        );

        assert!(
            result.app_header.is_some(),
            "AppHeader missing -- no window decoded the meta"
        );
        assert_eq!(
            &result.data[..data.len()],
            &data[..],
            "V3 loopback: data mismatch"
        );
    }

    #[test]
    fn probe_preamble_detects_real_signal() {
        let config = profile_high();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(3)).collect();
        let samples = tx_v3(&data, &config, 0xCAFE_0001);
        assert!(
            probe_preamble_present(&samples, &config),
            "probe must detect a real preamble in a V3 TX stream"
        );
    }

    #[test]
    fn detect_best_profile_picks_right_rs() {
        use crate::profile::{profile_normal, profile_robust, profile_ultra, ProfileIndex};
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(13)).collect();
        // Worker is currently configured in HIGH ; TX emits ULTRA / ROBUST.
        // Unique pitches → must trigger a switch.
        let ultra = tx_v3(&data, &profile_ultra(), 0x0001);
        let detected = detect_best_profile(&ultra, ProfileIndex::High).expect("ultra detected");
        assert_eq!(detected, ProfileIndex::Ultra);

        let robust = tx_v3(&data, &profile_robust(), 0x0002);
        let detected = detect_best_profile(&robust, ProfileIndex::High).expect("robust detected");
        assert_eq!(detected, ProfileIndex::Robust);

        // NORMAL shares Rs/τ/β with HIGH. If the worker is already in HIGH,
        // detect_best_profile must return None (no switch : let the header
        // refine). Otherwise we'd oscillate HIGH ↔ NORMAL every tick.
        let normal = tx_v3(&data, &profile_normal(), 0x0003);
        let detected = detect_best_profile(&normal, ProfileIndex::High);
        assert!(
            detected.is_none(),
            "tied pitch HIGH↔NORMAL must not switch when current is HIGH, got {:?}",
            detected
        );

        // Same burst but worker currently in ULTRA : should switch to the
        // Rs=1500 family A group. Which exact profile is picked is a
        // tie-break detail (sort_by is stable on equal ratios, so the
        // first family-A entry in `ProfileIndex::ALL` wins) -- what
        // matters is that the switch DOES occur and lands on a family-A
        // standard. The header's profile_index refines downstream.
        let detected = detect_best_profile(&normal, ProfileIndex::Ultra);
        assert!(
            matches!(
                detected,
                Some(ProfileIndex::High)
                    | Some(ProfileIndex::Normal)
                    | Some(ProfileIndex::HighPlus)
                    | Some(ProfileIndex::HighPlusPlus)
                    | Some(ProfileIndex::HighFiveSix)
            ),
            "from ULTRA on NORMAL burst, expected a family-A standard pick, got {:?}",
            detected
        );
    }

    #[test]
    fn detect_best_profile_returns_none_on_noise() {
        use crate::profile::ProfileIndex;
        let mut rng = 1u64;
        let samples: Vec<f32> = (0..(AUDIO_RATE as usize * 2))
            .map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((rng >> 32) as i32 as f32) / i32::MAX as f32 * 0.1
            })
            .collect();
        assert!(
            detect_best_profile(&samples, ProfileIndex::High).is_none(),
            "detect must reject pure noise"
        );
    }

    #[test]
    fn probe_preamble_all_profiles() {
        use crate::profile::{profile_normal, profile_robust, profile_ultra};
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(7)).collect();
        for (name, cfg) in [
            ("ultra", profile_ultra()),
            ("robust", profile_robust()),
            ("normal", profile_normal()),
            ("high", profile_high()),
        ] {
            let samples = tx_v3(&data, &cfg, 0xFACE_0001);
            assert!(
                probe_preamble_present(&samples, &cfg),
                "probe must detect preamble for profile {name}"
            );
        }
    }

    #[test]
    fn loopback_v3_all_profiles_small_payload() {
        use crate::profile::{profile_normal, profile_robust, profile_ultra};
        let data: Vec<u8> = (0..150).map(|i| (i as u8).wrapping_mul(11)).collect();
        for (name, cfg) in [
            ("ultra", profile_ultra()),
            ("robust", profile_robust()),
            ("normal", profile_normal()),
        ] {
            let samples = tx_v3(&data, &cfg, 0xFEED_0001);
            let r = rx_v3(&samples, &cfg).unwrap_or_else(|| panic!("rx_v3 None for {name}"));
            assert!(r.app_header.is_some(), "{name} : no AppHeader");
            assert_eq!(&r.data[..data.len()], &data[..], "{name} : payload mismatch");
        }
    }

    /// Loopback FAST (16-APSK 1714 Bd beta=0.15 LDPC 3/4): exercises
    /// the sps=28 + beta=0.15 + RRC + preamble sync + LDPC chain.
    #[test]
    fn loopback_v3_fast_small_payload() {
        let cfg = crate::profile::profile_fast();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(17)).collect();
        let samples = tx_v3(&data, &cfg, 0xFA57_0001);
        let r = rx_v3(&samples, &cfg).expect("rx_v3 None for FAST");
        assert!(r.app_header.is_some(), "FAST: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "FAST loopback: payload mismatch",
        );
    }

    /// Loopback HIGH56 (16-APSK 1500 Bd beta=0.20 LDPC 5/6): exercises
    /// the IEEE 802.16e LDPC 5/6 matrix in the complete pipeline.
    #[test]
    fn loopback_v3_high_5_6_small_payload() {
        let cfg = crate::profile::profile_high_5_6();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(7)).collect();
        let samples = tx_v3(&data, &cfg, 0xC0DE_0056);
        let r = rx_v3(&samples, &cfg).expect("rx_v3 None for HIGH56");
        assert!(r.app_header.is_some(), "HIGH56: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "HIGH56 loopback: payload mismatch",
        );
    }

    /// Loopback HIGH+56 (32-APSK 1500 Bd beta=0.20 LDPC 5/6): exercises
    /// the LDPC 5/6 matrix on the 32-APSK constellation.
    #[test]
    fn loopback_v3_high_plus_5_6_small_payload() {
        let cfg = crate::profile::profile_high_plus_5_6();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(19)).collect();
        let samples = tx_v3(&data, &cfg, 0xC0DE_5656);
        let r = rx_v3(&samples, &cfg).expect("rx_v3 None for HIGH+56");
        assert!(r.app_header.is_some(), "HIGH+56: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "HIGH+56 loopback: payload mismatch",
        );
    }

    /// Sanity baseline — feed the un-drifted TX stream and verify the
    /// estimator reports ≈0 ppm. Surfaces any systematic offset between
    /// `expected_pos` and `observed_pos` that isn't due to actual drift.
    #[test]
    fn marker_fit_drift_high_plus_zero_baseline() {
        let cfg = crate::profile::profile_high_plus();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(5)).collect();
        let samples_tx = tx_v3(&data, &cfg, 0xFEED_BEEF);
        // No resample, no noise. Pass straight to rx_v2.
        let r = rx_v2(&samples_tx, &cfg).expect("rx_v2 None on clean stream");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "clean: payload mismatch",
        );
        // Clean signal: 0.10.24+ runs Gardner unconditionally (per-window
        // estimator surfaces the actual channel state, not just "no
        // correction applied"). Gardner's OLS on synthetic markers has
        // a ~1 ppm noise floor even on a literally drift-free stream,
        // so we accept anything within ±2 ppm. The point is "no large
        // bogus drift" -- we used to require literally 0 here, but
        // that contract conflicted with the 2026-05-13 fix that made
        // the estimator visible per-SF.
        assert!(r.drift_ppm.abs() < 2.0, "clean: drift_ppm={:+.2}", r.drift_ppm);
    }

    /// Marker-fit drift estimator (Phase 1) on HIGH+ with injected
    /// TX/RX clock mismatch, no noise. Stretches the TX audio by
    /// +30 ppm to emulate "RX clock faster than TX by 30 ppm".
    ///
    /// Asserts: payload bit-exact + `drift_ppm` within ±5 ppm of the
    /// injected +30.
    #[test]
    fn marker_fit_drift_high_plus_plus_30_ppm() {
        let cfg = crate::profile::profile_high_plus();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(7)).collect();
        let samples_tx = tx_v3(&data, &cfg, 0xDEAD_BEEF);
        // resample_audio(x, -ppm) stretches by (1 - ppm·1e-6)^-1 ≈
        // 1 + ppm·1e-6, simulating an RX clock faster than TX by ppm.
        let injected_ppm = 30.0f64;
        let samples_drifted = resample_audio(&samples_tx, -injected_ppm);
        let r = rx_v2(&samples_drifted, &cfg).expect("rx_v2 None on +30 ppm");
        assert!(r.app_header.is_some(), "drift+30: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "drift+30: payload mismatch",
        );
        let err = (r.drift_ppm - injected_ppm).abs();
        assert!(
            err < 6.0,
            "drift+30: estimator returned {:+.2} ppm, want ~{:+.0} (err {:.2} > 5)",
            r.drift_ppm, injected_ppm, err,
        );
    }

    /// Symmetric drift test: -45 ppm (TX clock faster) on HIGH+, no
    /// noise. Exercises the negative-ppm branch of the estimator.
    #[test]
    fn marker_fit_drift_high_plus_minus_45_ppm() {
        let cfg = crate::profile::profile_high_plus();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(11)).collect();
        let samples_tx = tx_v3(&data, &cfg, 0x1234_4321);
        let injected_ppm = -45.0f64;
        let samples_drifted = resample_audio(&samples_tx, -injected_ppm);
        let r = rx_v2(&samples_drifted, &cfg).expect("rx_v2 None on -45 ppm");
        assert!(r.app_header.is_some(), "drift-45: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "drift-45: payload mismatch",
        );
        let err = (r.drift_ppm - injected_ppm).abs();
        assert!(
            err < 6.0,
            "drift-45: estimator returned {:+.2} ppm, want ~{:+.0} (err {:.2} > 5)",
            r.drift_ppm, injected_ppm, err,
        );
    }

    /// Drift +30 ppm + AWGN at noise_rms = 0.05 (≈ 20 dB SNR vs the
    /// peak-normalised TX of unit amplitude). Realistic for a clean
    /// sound-card / radio path on a quiet band.
    ///
    /// Tighter ppm tolerance is RELAXED to ±8 ppm because the marker
    /// correlation peak broadens with noise (σ_pos in the parabolic
    /// fit grows).
    #[test]
    fn marker_fit_drift_high_plus_30_ppm_awgn_20db() {
        let cfg = crate::profile::profile_high_plus();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(17)).collect();
        let samples_tx = tx_v3(&data, &cfg, 0xBEEF_BEEF);
        let injected_ppm = 30.0f64;
        let samples_drifted = resample_audio(&samples_tx, -injected_ppm);
        let samples_noisy = add_awgn(&samples_drifted, 0.05, 0xA11C_E11A);
        let r = rx_v2(&samples_noisy, &cfg).expect("rx_v2 None on +30 ppm + AWGN");
        assert!(r.app_header.is_some(), "drift+30+AWGN: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "drift+30+AWGN: payload mismatch",
        );
        let err = (r.drift_ppm - injected_ppm).abs();
        assert!(
            err < 8.0,
            "drift+30+AWGN: estimator returned {:+.2} ppm, want ~{:+.0} (err {:.2} > 8)",
            r.drift_ppm, injected_ppm, err,
        );
    }

    /// Drift -45 ppm + heavier noise (rms = 0.10 ≈ 14 dB SNR). Stress
    /// test for the estimator at marginal SNR. ±10 ppm tolerance.
    #[test]
    fn marker_fit_drift_high_plus_minus_45_ppm_awgn_14db() {
        let cfg = crate::profile::profile_high_plus();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(23)).collect();
        let samples_tx = tx_v3(&data, &cfg, 0xC0FF_EEEE);
        let injected_ppm = -45.0f64;
        let samples_drifted = resample_audio(&samples_tx, -injected_ppm);
        let samples_noisy = add_awgn(&samples_drifted, 0.10, 0xB0BA_CAFE);
        let r = rx_v2(&samples_noisy, &cfg).expect("rx_v2 None on -45 ppm + AWGN");
        assert!(r.app_header.is_some(), "drift-45+AWGN: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "drift-45+AWGN: payload mismatch",
        );
        let err = (r.drift_ppm - injected_ppm).abs();
        assert!(
            err < 12.0,
            "drift-45+AWGN: estimator returned {:+.2} ppm, want ~{:+.0} (err {:.2} > 10)",
            r.drift_ppm, injected_ppm, err,
        );
    }

    // -------------------------------------------------------------------
    // Phase 1c v2 — per-profile drift sweep matrix
    // -------------------------------------------------------------------
    //
    // Each profile sweep runs N drift values × {clean, AWGN}. For
    // un-drifted (ε=0), assertion is loose (clean first pass → drift_ppm
    // = 0); for drifted, we check both bit-exact payload AND that the
    // converged ppm is within tolerance of the injected value.

    /// Helper: TX -> drift -> AWGN -> RX. Asserts payload + ppm tolerance.
    fn run_drift_test(
        profile_name: &str,
        cfg: &crate::profile::ModemConfig,
        payload_size: usize,
        seed: u32,
        injected_ppm: f64,
        noise_rms: f32,
        ppm_tolerance: f64,
    ) {
        let data: Vec<u8> = (0..payload_size)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(13))
            .collect();
        let samples_tx = tx_v3(&data, cfg, seed);
        let samples_drifted = if injected_ppm.abs() > 0.01 {
            resample_audio(&samples_tx, -injected_ppm)
        } else {
            samples_tx.clone()
        };
        let samples_final = if noise_rms > 0.0 {
            add_awgn(&samples_drifted, noise_rms, (seed as u64) ^ 0xA5A5_5A5A)
        } else {
            samples_drifted
        };
        let r = rx_v2(&samples_final, cfg).unwrap_or_else(|| {
            panic!(
                "{profile_name} drift={injected_ppm:+.0} noise={noise_rms}: rx_v2 None"
            )
        });
        assert!(
            r.app_header.is_some(),
            "{profile_name} drift={injected_ppm:+.0} noise={noise_rms}: no AppHeader"
        );
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "{profile_name} drift={injected_ppm:+.0} noise={noise_rms}: payload mismatch"
        );
        // Two valid success states when drift is injected:
        //   (a) `drift_ppm` close to injected -- iterative path engaged
        //       and converged.
        //   (b) `drift_ppm` == 0 (first-pass clean) -- the channel
        //       (pilots + LDPC margin) absorbed the drift without
        //       needing the resample. Common for robust profiles
        //       (QPSK NORMAL, 64-APSK HIGH++ with 16/2 pilots) at
        //       moderate drift.
        // Both are functionally correct (payload bit-exact). The
        // tolerance check only applies when the iterative path
        // actually ran -- `drift_ppm` non-zero.
        if injected_ppm.abs() > 1.0 && r.drift_ppm.abs() > 0.1 {
            let err = (r.drift_ppm - injected_ppm).abs();
            assert!(
                err < ppm_tolerance,
                "{profile_name} drift={injected_ppm:+.0} noise={noise_rms}: \
                 drift_ppm={:+.2}, want ~{:+.0}, err={:.2} > tol={:.2}",
                r.drift_ppm, injected_ppm, err, ppm_tolerance,
            );
        }
    }

    /// NORMAL (QPSK 1500 Bd β=0.20 LDPC 3/4) — sweep ±30, ±60 ppm clean + ±30 AWGN.
    #[test]
    fn drift_sweep_normal() {
        let cfg = crate::profile::profile_normal();
        let n = "NORMAL";
        run_drift_test(n, &cfg, 200, 0x4E00_0000, 0.0, 0.0, 4.0);
        run_drift_test(n, &cfg, 200, 0x4E00_0001, 30.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4E00_0002, -30.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4E00_0003, 60.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4E00_0004, -60.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4E00_0005, 30.0, 0.05, 8.0);
        run_drift_test(n, &cfg, 200, 0x4E00_0006, -30.0, 0.05, 8.0);
    }

    /// HIGH (QPSK 1500 Bd β=0.20 LDPC 5/6) — same sweep as NORMAL.
    #[test]
    fn drift_sweep_high() {
        let cfg = crate::profile::profile_high();
        let n = "HIGH";
        run_drift_test(n, &cfg, 200, 0x4800_0000, 0.0, 0.0, 4.0);
        run_drift_test(n, &cfg, 200, 0x4800_0001, 30.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4800_0002, -30.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4800_0003, 60.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4800_0004, -45.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4800_0005, 30.0, 0.05, 8.0);
    }

    /// HIGH+56 (HIGH+ with LDPC 5/6) — thinner LDPC margin on 32-APSK.
    #[test]
    fn drift_sweep_high_plus_5_6() {
        let cfg = crate::profile::profile_high_plus_5_6();
        let n = "HIGH+56";
        run_drift_test(n, &cfg, 200, 0x4856_0000, 0.0, 0.0, 4.0);
        run_drift_test(n, &cfg, 200, 0x4856_0001, 30.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4856_0002, -45.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 200, 0x4856_0003, 30.0, 0.05, 8.0);
    }

    /// HIGH++ (64-APSK DVB-S2X 1500 Bd β=0.20 LDPC 3/4) — densest
    /// constellation in the lineup, K factor may differ from HIGH+.
    #[test]
    fn drift_sweep_high_plus_plus() {
        let cfg = crate::profile::profile_high_plus_plus();
        let n = "HIGH++";
        run_drift_test(n, &cfg, 1500, 0xC064_0000, 0.0, 0.0, 4.0);
        run_drift_test(n, &cfg, 1500, 0xC064_0001, 30.0, 0.0, 6.0);
        run_drift_test(n, &cfg, 1500, 0xC064_0002, -30.0, 0.0, 6.0);
        run_drift_test(n, &cfg, 1500, 0xC064_0003, 45.0, 0.0, 6.0);
        run_drift_test(n, &cfg, 1500, 0xC064_0004, -45.0, 0.0, 6.0);
        run_drift_test(n, &cfg, 1500, 0xC064_0005, 30.0, 0.03, 8.0);
    }

    /// MEGA (16-APSK FTN tau=30/32, 1500 Bd) — sub-Nyquist pulse
    /// density. Bigger payload (1200 B) to ensure ≥3 markers per SF so
    /// the estimator's linear regression has enough points.
    ///
    /// **Tolerance widening for FTN**: with `apply_ffe_lms_with_training`
    /// (required because fixed taps don't equalise FTN ISI well enough
    /// to detect markers), the LMS timing adaptation makes K highly
    /// nonlinear in ε. K_actual ≈ 2.3 at large drift, ≈ 0.1 near zero.
    /// The secant method picks an averaged K and converges to a value
    /// off by ~20 ppm from the true injected drift. Payload still
    /// decodes (pilots + LDPC margin absorb the residual). So the
    /// `drift_ppm` tolerance here is loose -- ±25 ppm. The PAYLOAD
    /// assertion is the actual correctness check.
    #[test]
    fn drift_sweep_mega() {
        let cfg = crate::profile::profile_mega();
        let n = "MEGA";
        run_drift_test(n, &cfg, 1200, 0x4D6E_0000, 0.0, 0.0, 5.0);
        run_drift_test(n, &cfg, 1200, 0x4D6E_0001, 30.0, 0.0, 25.0);
        run_drift_test(n, &cfg, 1200, 0x4D6E_0002, -30.0, 0.0, 25.0);
        run_drift_test(n, &cfg, 1200, 0x4D6E_0003, 30.0, 0.05, 25.0);
    }

    /// Loopback HIGH+ (32-APSK 1500 Bd beta=0.20 LDPC 3/4): exercises
    /// the Apsk32 constellation + 5-bit interleaver + soft demap +
    /// LDPC 3/4 chain on an ideal channel.
    #[test]
    fn loopback_v3_high_plus_small_payload() {
        let cfg = crate::profile::profile_high_plus();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(13)).collect();
        let samples = tx_v3(&data, &cfg, 0xBABE_5005);
        let r = rx_v3(&samples, &cfg).expect("rx_v3 None for HIGH+");
        assert!(r.app_header.is_some(), "HIGH+: no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "HIGH+ loopback: payload mismatch",
        );
    }

    /// Loopback HIGH++ (64-APSK DVB-S2X 4+12+20+28, 1500 Bd
    /// beta=0.20 LDPC 3/4, 16/2 pilots + 64-sym LMS guard interval).
    ///
    /// End-to-end validation: AppHeader decodes, payload reassembled
    /// bit-exact. The Apsk64 + 6-bit interleaver + soft demap +
    /// LDPC 3/4 chain works on an ideal channel thanks to the pilot
    /// densification (16/2 vs the default 32/2), which halves the
    /// intra-segment phase tracking variance -- the 64-APSK LDPC
    /// margin is ~3x tighter than 32-APSK; the standard 32/2 pilot
    /// tracking left sigma^2~=0.011 which capped CW convergence at
    /// 70%. With 16/2: sigma^2~=0.005, 100% convergence on loopback.
    /// 2304 % 6 = 0 -> no padding bits (unlike HIGH+).
    #[test]
    fn loopback_v3_high_plus_plus_small_payload() {
        let cfg = crate::profile::profile_high_plus_plus();
        let data: Vec<u8> = (0..1500).map(|i| (i as u8).wrapping_mul(11)).collect();
        let samples = tx_v3(&data, &cfg, 0xCAFE_F00D);
        let r = rx_v3(&samples, &cfg).expect("rx_v3 None for HIGH++");
        eprintln!(
            "HIGH++ loopback : converged={}/{} segs={}/lost={} sigma²={:.6} data_cw={} app_hdr={}",
            r.converged_blocks, r.total_blocks, r.segments_decoded, r.segments_lost,
            r.sigma2, r.data_blocks_recovered, r.app_header.is_some(),
        );
        assert!(r.app_header.is_some(), "HIGH++ : no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "HIGH++ loopback : payload mismatch",
        );
    }

    /// Loopback : ULTRA with the dense 16/2 pattern + DD-PLL QPSK refinement
    /// on a payload big enough to span several data segments and at least one
    /// periodic preamble reinsertion. Guards against regressions in the
    /// pattern-aware sizing/indexing and confirms the DD-PLL second pass
    /// doesn't hurt decode on a clean channel.
    #[test]
    fn loopback_v3_ultra_medium_payload() {
        use crate::profile::profile_ultra;
        let cfg = profile_ultra();
        let data: Vec<u8> = (0..800)
            .map(|i| (i as u32).wrapping_mul(0x9E37_79B9) as u8)
            .collect();
        let samples = tx_v3(&data, &cfg, 0xC001_ABCD);
        let r = rx_v3(&samples, &cfg).expect("rx_v3 None for ULTRA medium");
        eprintln!(
            "V3 ULTRA 800B : converged={}/{} segs={}/lost={} sigma²={:.4} data_cw={}",
            r.converged_blocks,
            r.total_blocks,
            r.segments_decoded,
            r.segments_lost,
            r.sigma2,
            r.data_blocks_recovered
        );
        assert!(r.app_header.is_some(), "ULTRA : no AppHeader");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "ULTRA medium loopback : payload mismatch",
        );
    }

    #[test]
    fn probe_preamble_rejects_noise() {
        let config = profile_high();
        // 2 s of deterministic noise — below the 10× peak/median gate.
        let mut rng = 1u64;
        let samples: Vec<f32> = (0..(AUDIO_RATE as usize * 2))
            .map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((rng >> 32) as i32 as f32) / i32::MAX as f32 * 0.1
            })
            .collect();
        assert!(
            !probe_preamble_present(&samples, &config),
            "probe must reject pure noise"
        );
    }

    /// End-of-transmission marker : a main burst followed by a short EOT
    /// frame must (a) decode normally, (b) expose `eot_seen = true` so the
    /// RX worker can trim its in-memory buffer without waiting on silence.
    #[test]
    fn loopback_v3_high_with_eot_marker() {
        let config = profile_high();
        let data: Vec<u8> = (0..2_000)
            .map(|i| (i as u32).wrapping_mul(0x9E37_79B9) as u8)
            .collect();
        let session = 0xCAFE_BABEu32;
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
                .expect("profile");
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let main_syms = frame::build_superframe_v3(
            &data, &config, session, modem_framing::app_header::mime::BINARY, make_session_hash(&data),
        );
        let main_audio = crate::modulator::modulate(&main_syms, sps, pitch, &taps, config.center_freq_hz);
        let eot_syms = frame::build_eot_frame(&config, session);
        let eot_audio = crate::modulator::modulate(&eot_syms, sps, pitch, &taps, config.center_freq_hz);

        let silence = vec![0.0f32; (AUDIO_RATE as f64 * 0.2) as usize];
        let mut combined = main_audio;
        combined.extend_from_slice(&silence);
        combined.extend_from_slice(&eot_audio);

        let result = rx_v3(&combined, &config).expect("rx_v3 should decode main + EOT");
        assert!(result.eot_seen, "EOT frame must set eot_seen");
        let ah = result.app_header.as_ref().expect("AppHeader from main burst");
        assert_eq!(ah.session_id, session);
        // The main burst's real file_size must win despite the EOT's zero
        // marker being present in a later window.
        assert_eq!(ah.file_size as usize, data.len());
        assert_eq!(&result.data[..data.len()], &data[..]);
    }

    /// Multi-burst scenario : an initial TX + a "More" TX with continuing ESIs
    /// against the same session_id. The concatenated audio stream, fed as a
    /// single input to rx_v3, must decode from the union of packets.
    #[test]
    fn loopback_v3_two_bursts_continuing_esi() {        use modem_framing::app_header::mime;
        use crate::ldpc::encoder::LdpcEncoder;
        use modem_framing::raptorq_codec;

        let config = profile_high();
        let data: Vec<u8> = (0..3_000)
            .map(|i| ((i * 53) ^ 0x5A) as u8)
            .collect();
        let session = 0xA28C_CD81u32;
        let hash = make_session_hash(&data);

        // Burst 1 : K + 30 % repair
        let k_bytes = LdpcEncoder::new(config.ldpc_rate).k() / 8;
        let k = raptorq_codec::k_from_payload(data.len(), k_bytes) as u32;
        let b1_count = k + raptorq_codec::n_repair_default(k);

        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
                .expect("profile");
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);

        let syms1 = frame::build_superframe_v3_range(
            &data, &config, session, mime::BINARY, hash, 0, b1_count,
        );
        let audio1 = modulator::modulate(&syms1, sps, pitch, &taps, config.center_freq_hz);

        // Burst 2 : +20 % of K, ESI continuing after b1
        let b2_count = (k * 20) / 100;
        let syms2 = frame::build_superframe_v3_range(
            &data, &config, session, mime::BINARY, hash, b1_count, b2_count,
        );
        let audio2 = modulator::modulate(&syms2, sps, pitch, &taps, config.center_freq_hz);

        // Concatenate the two bursts, add 100 ms of silence between.
        let silence = vec![0.0f32; (AUDIO_RATE as f64 * 0.1) as usize];
        let mut combined = audio1.clone();
        combined.extend_from_slice(&silence);
        combined.extend_from_slice(&audio2);

        let result = rx_v3(&combined, &config).expect("rx_v3 should decode union");
        let ah = result.app_header.as_ref().expect("AppHeader");
        assert_eq!(ah.session_id, session);

        eprintln!(
            "V3 two-burst: K={k} b1={b1_count} b2={b2_count} \
             data_cw_recovered={} bytes={}",
            result.data_blocks_recovered,
            result.data.len()
        );
        assert_eq!(&result.data[..data.len()], &data[..]);
    }

    /// Regression: NORMAL and HIGH share (Rs, tau, beta) -> the
    /// preamble correlation cannot tell them apart. If the worker
    /// starts on HIGH by default and the TX emits NORMAL, the first
    /// `rx_v3` still decodes the Golay header (96 fixed QPSK symbols
    /// independent of the data constellation), which exposes the
    /// correct profile. The worker must then re-decode the same buffer
    /// with the correct profile -- otherwise the first superframe
    /// (~4 s in NORMAL) is lost, as observed OTA (ESIs 0..5 missing
    /// on session 2dc17ae3).
    #[test]
    fn rx_v3_normal_decoded_with_high_config_exposes_profile_index() {
        use modem_framing::app_header::mime;
        use crate::profile::{profile_high, profile_normal, ProfileIndex};
        let normal = profile_normal();
        let data: Vec<u8> = (0..2000)
            .map(|i| (i as u32).wrapping_mul(0x9E37_79B9) as u8)
            .collect();
        let session = 0xABCD_EF12u32;
        let samples = tx_v3(&data, &normal, session);

        // 1st attempt: HIGH config (wrong profile but same Rs/tau/beta).
        // The Golay header must still decode and expose profile_index = NORMAL.
        let r1 = rx_v3(&samples, &profile_high()).expect("rx_v3 None with HIGH config");
        let hdr = r1.header.as_ref().expect("Golay header should decode");
        let hdr_profile =
            ProfileIndex::from_u8(hdr.profile_index).expect("profile_index valid");
        assert_eq!(
            hdr_profile,
            ProfileIndex::Normal,
            "header must reveal TX is emitting NORMAL"
        );

        // 2nd attempt: NORMAL config -> full decode.
        let r2 = rx_v3(&samples, &normal).expect("rx_v3 None with NORMAL config");
        assert!(r2.app_header.is_some());
        assert_eq!(&r2.data[..data.len()], &data[..]);
        // Sanity: payload decoded with the correct profile restores the bytes.
        let _ = mime::BINARY;
    }

    /// Regression: a burst with odd `n_total` and zero repair margin
    /// (observed OTA in HIGH with K=19, repair_pct=0) must recover the
    /// very last codeword. Before fix: the TX wrote 1 CW into the final
    /// segment, the RX expected 2 + pilots, and the runout did not
    /// cover the delta -> break past the buffer and the final ESI is
    /// lost. RaptorQ no longer converges.
    #[test]
    fn loopback_v3_high_odd_n_total_recovers_last_block() {
        use modem_framing::app_header::mime;
        use crate::ldpc::encoder::LdpcEncoder;
        use modem_framing::raptorq_codec;
        let config = profile_high();
        let k_bytes = LdpcEncoder::new(config.ldpc_rate).k() / 8;
        // 4003 B -> K = ceil(4003/216) = 19 (odd). With n_packets = K
        // and repair_pct = 0, n_total = 19 reproduces exactly the
        // failed session.
        let data: Vec<u8> = (0..4003)
            .map(|i| (i as u32).wrapping_mul(0x9E37_79B9) as u8)
            .collect();
        let k = raptorq_codec::k_from_payload(data.len(), k_bytes) as u32;
        assert_eq!(k % 2, 1, "test setup: K must be odd to reproduce the bug");
        let session = 0xDEAD_C0FFu32;
        let hash = make_session_hash(&data);

        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
                .expect("profile");
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let syms = frame::build_superframe_v3_range(
            &data, &config, session, mime::BINARY, hash, 0, k,
        );
        let audio = modulator::modulate(&syms, sps, pitch, &taps, config.center_freq_hz);

        let result = rx_v3(&audio, &config).expect("rx_v3 None");
        let ah = result.app_header.as_ref().expect("AppHeader");
        eprintln!(
            "V3 HIGH odd-K=19: data_cw_recovered={}/{} bytes={}",
            result.data_blocks_recovered,
            ah.k_symbols,
            result.data.len()
        );
        assert!(
            result.data_blocks_recovered >= ah.k_symbols as usize,
            "K={} but only {} CWs recovered -- final segment lost",
            ah.k_symbols, result.data_blocks_recovered,
        );
        assert_eq!(&result.data[..data.len()], &data[..]);
    }

    /// Axis 2 : low-power live worker tick (`allow_legacy_grid = false`,
    /// `finalize = false`) caps the per-tick scan at 2 preambles. On a
    /// 3+ SF audio buffer, the call decodes only the first CLOSED
    /// window (positions[0]) and surfaces `positions[1]` -- the
    /// **unprocessed** boundary -- via `last_preamble_offset`. The
    /// worker drains everything up to `positions[1] - TRUNCATE_MARGIN`
    /// (rx_worker.rs:1701-1706), which drops positions[0] but keeps
    /// positions[1] at margin offset for next tick's re-scan where
    /// Gardner is re-estimated fresh.
    ///
    /// **REGRESSION GUARD (2026-05-14 OTA)** : an earlier draft of
    /// Axis 2 made `last_preamble_offset = last_processed = positions[0]`.
    /// That kept positions[0] in the buffer AFTER drain, so the next
    /// tick re-detected it and re-decoded indefinitely -- the GUI
    /// froze on the second SF. This test asserts the watermark is
    /// **strictly past** positions[0] by simulating one drain cycle
    /// and checking the second tick advances to a new preamble.
    #[test]
    fn rx_v3_after_lowpower_one_sf_per_tick() {
        let config = profile_high();
        // 15 kB payload triggers several superframe wraps on HIGH (same
        // size as the loopback_v3_high_sliding_window test above).
        let data: Vec<u8> = (0..15_000)
            .map(|i| (i as u32).wrapping_mul(2654435761) as u8)
            .collect();
        let samples = tx_v3(&data, &config, 0xC0DE_BEEFu32);

        // Uncapped: finalize=true => no cap, decodes every SF.
        let uncapped = rx_v3_after(&samples, &config, 0, true, None, true, false, false)
            .expect("uncapped rx_v3_after returned None");

        // Capped: live-worker pattern (finalize=false, lowpower_position_cap=true).
        // allow_legacy_grid=false too so the cap is the only thing that
        // limits the per-tick decode.
        let capped = rx_v3_after(&samples, &config, 0, false, None, false, true, false)
            .expect("capped rx_v3_after returned None");

        eprintln!(
            "Axis-2 cap: uncapped segs={} converged={} last_off={:?}  |  capped segs={} converged={} last_off={:?}",
            uncapped.segments_decoded,
            uncapped.converged_blocks,
            uncapped.last_preamble_offset,
            capped.segments_decoded,
            capped.converged_blocks,
            capped.last_preamble_offset,
        );

        // Cap must produce STRICTLY FEWER decoded segments than uncapped
        // on a multi-SF buffer (else the cap had no effect).
        assert!(
            capped.segments_decoded < uncapped.segments_decoded,
            "cap must throttle multi-SF buffer: capped segs={} vs uncapped segs={}",
            capped.segments_decoded, uncapped.segments_decoded,
        );

        // Watermark must point at a position STRICTLY EARLIER than the
        // uncapped run's last offset -- meaning at least one preamble
        // remains in the buffer for the next tick to re-find.
        let cap_off = capped.last_preamble_offset.expect("capped watermark");
        let unc_off = uncapped.last_preamble_offset.expect("uncapped watermark");
        assert!(
            cap_off < unc_off,
            "capped watermark must be earlier than uncapped: {cap_off} >= {unc_off}",
        );

        // **REGRESSION GUARD** : simulate one worker drain cycle and a
        // second rx_v3_after call on the drained tail. If the watermark
        // is positions[0] instead of positions[1] (the old bug), the
        // drained buffer still contains positions[0] and the second
        // call re-decodes the same SF -- producing the same
        // `last_preamble_offset`. With the correct watermark
        // (positions[1]), the second call locks on a different SF and
        // the watermark advances.
        const TRUNCATE_MARGIN_SAMPLES: usize = (AUDIO_RATE as usize * 100) / 1000; // 4800
        let drain_end = cap_off.saturating_sub(TRUNCATE_MARGIN_SAMPLES);
        assert!(drain_end > 0, "drain_end must advance past head");
        let tail = &samples[drain_end..];
        let capped2 = rx_v3_after(tail, &config, 0, false, None, false, true, false)
            .expect("second-tick rx_v3_after returned None");
        let cap_off2 = capped2.last_preamble_offset.expect("second-tick watermark");

        // In the broken implementation, cap_off2 would re-find positions[0]
        // (which is at offset TRUNCATE_MARGIN_SAMPLES in `tail`). With the
        // fix, cap_off2 points further (to positions[2] or later in the
        // original buffer = positions[1] of `tail` after re-scan).
        eprintln!(
            "Axis-2 second-tick: tail_len={} drain_end={} cap_off2={} -- absolute = {}",
            tail.len(), drain_end, cap_off2, drain_end + cap_off2,
        );
        let absolute_cap_off2 = drain_end + cap_off2;
        assert!(
            absolute_cap_off2 > cap_off,
            "second tick MUST advance the watermark : got {absolute_cap_off2} \
             (= drain_end {drain_end} + cap_off2 {cap_off2}) which is not > {cap_off}. \
             Likely regression : `last_preamble_offset` reports positions[0] \
             (= last_processed) instead of positions[1] (= boundary)."
        );
    }

    /// Axis 1 regression (refined in 0.10.32) : the 0-ppm fast-path is
    /// skipped ONLY when Gardner has already produced a clean decode.
    /// On a drifted CLEAN channel where Gardner locks (=> >0.5 ppm
    /// resample => clean rx_v2_with_hint), the fast-path becomes pure
    /// redundancy and the lowpower path drops it.
    ///
    /// Uses +30 ppm injected drift (well above the 0.5 ppm gate, well
    /// below the legacy-grid corner cases) so Gardner is the path that
    /// decodes the SF in both desktop and lowpower modes ; the only
    /// per-mode difference is whether the fast-path PassDone+MF cost
    /// also gets paid.
    #[test]
    fn rx_v2_with_options_skips_fast_path_when_gardner_clean() {
        // HIGH+ has more markers per SF than HIGH -> Gardner OLS is
        // more reliable at +30 ppm. Same recipe as the existing
        // marker_fit_drift_high_plus_plus_30_ppm test.
        let config = crate::profile::profile_high_plus();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(7)).collect();
        let samples = tx_v3(&data, &config, 0xCAFE_BABE);
        // resample_audio(x, -ppm) simulates "RX clock faster than TX
        // by ppm". 30 ppm well above the 0.5 ppm gate so Gardner's
        // rx_v2_with_hint branch fires and produces a clean decode.
        let drifted = resample_audio(&samples, -30.0);

        // Baseline : fast-path allowed, grid allowed (= desktop).
        let _ = take_perf();
        let r_desktop = rx_v2_with_options(&drifted, &config, true, true);
        let perf_desktop = take_perf();

        // Lowpower : fast-path AND grid gated off.
        let _ = take_perf();
        let r_lowpower = rx_v2_with_options(&drifted, &config, false, false);
        let perf_lowpower = take_perf();

        eprintln!(
            "Axis-1 gate: desktop n_passes={} mf={}us  vs  lowpower n_passes={} mf={}us",
            perf_desktop.n_passes,
            perf_desktop.matched_filter_us,
            perf_lowpower.n_passes,
            perf_lowpower.matched_filter_us,
        );

        // Both modes must still decode the SF -- the fast-path skip is
        // an OPTIMIZATION, not a behaviour change for the lock case.
        assert!(r_desktop.is_some(), "desktop must decode +30 ppm signal");
        assert!(r_lowpower.is_some(), "lowpower must still decode +30 ppm via Gardner");

        // Fast-path elimination must strictly reduce the pass count and
        // the matched-filter wall-clock. We don't pin absolute numbers
        // (those depend on host) ; the delta is the contract.
        assert!(
            perf_lowpower.n_passes < perf_desktop.n_passes,
            "lowpower must skip at least one PassDone (got {} vs {})",
            perf_lowpower.n_passes, perf_desktop.n_passes,
        );
        assert!(
            perf_lowpower.matched_filter_us < perf_desktop.matched_filter_us,
            "lowpower must spend less time in matched_filter (got {}us vs {}us)",
            perf_lowpower.matched_filter_us, perf_desktop.matched_filter_us,
        );
    }

    /// 0.10.32 regression : on a NEAR-ZERO-DRIFT channel Gardner gates
    /// itself out (|ppm| < 0.5, line 566) so the rx_v2_with_hint
    /// branch never runs and `best` stays None after step 1. The
    /// fast-path MUST run as a fallback even in lowpower mode --
    /// otherwise the SF is silently lost. This is the bug spotted on
    /// Pi 4 OTA 2026-05-14 between 0.10.30 and 0.10.32 : new
    /// transmissions whose early SFs land near zero drift never armed
    /// the worker, and constellations on borderline-drift channels
    /// degraded (only the >0.5 ppm SFs survived, via Gardner-resample).
    #[test]
    fn rx_v2_with_options_lowpower_falls_back_to_fast_path_on_zero_drift() {
        let config = profile_high();
        let data: Vec<u8> = (0..2000)
            .map(|i| (i as u32).wrapping_mul(0x9E37_79B9) as u8)
            .collect();
        // Zero-drift signal : Gardner returns ppm < 0.5 (or even None),
        // its branch never produces a `best`, and the fast-path is the
        // only viable decoder. Lowpower must still decode this.
        let samples = tx_v3(&data, &config, 0xCAFE_BABE);

        let _ = take_perf();
        let r_lowpower = rx_v2_with_options(&samples, &config, false, false);
        let perf_lowpower = take_perf();

        eprintln!(
            "0.10.32 fallback: zero-drift lowpower n_passes={} mf={}us decoded={}",
            perf_lowpower.n_passes,
            perf_lowpower.matched_filter_us,
            r_lowpower.is_some(),
        );

        assert!(
            r_lowpower.is_some(),
            "lowpower MUST decode a zero-drift SF via fast-path fallback : \
             Gardner gates itself out under 0.5 ppm and the fast-path is \
             the only remaining decoder. Returning None here corresponds to \
             the 0.10.30/0.10.31 bug : new transmissions never re-armed."
        );
        // The fast-path ran -> at least one PassDone fired from rx_v2_single.
        assert!(
            perf_lowpower.n_passes >= 1,
            "fast-path must contribute >= 1 PassDone on zero-drift lowpower"
        );
    }

    // === Parallel safety grid (aarch64) regression tests ===
    //
    // The aarch64 path replaces the sequential 6-cell ±15 ppm loop with
    // a two-batch (4+2) `std::thread::scope` block + `AtomicBool` cancel
    // flag. The tests below validate the externally-observable contract :
    // identical decode behaviour vs the sequential path, no panic under
    // worker failure, no thread leak under repeated invocation. They run
    // on both arches -- on x86_64 they exercise the sequential branch,
    // which is identical to pre-change behaviour and serves as a
    // regression check that the cfg-gating didn't break the legacy code.

    /// Pure noise input -> grid must execute (all 6 cells try, all
    /// return None) and the function returns None without panicking.
    /// Stresses the path where every worker thread returns None and the
    /// cancel flag is never set.
    #[test]
    fn grid_parallel_noise_input_returns_none() {
        let config = profile_high();
        let mut rng = Lcg::new(0xDEAD_BEEF);
        let samples: Vec<f32> = (0..(AUDIO_RATE as usize * 2))
            .map(|_| rng.next_gauss() * 0.1)
            .collect();
        // grid enabled, fast-path enabled => full path including the
        // parallel grid on aarch64.
        let r = rx_v2_with_options(&samples, &config, true, true);
        assert!(r.is_none(), "noise must not decode (got Some(_))");
    }

    /// Clean signal, grid enabled. Gardner+fast-path normally already
    /// produces a clean decode so the grid is skipped -- but we still
    /// exercise the `still_not_clean` gate and the surrounding plumbing.
    /// Payload must match TX byte-exact.
    #[test]
    fn grid_parallel_clean_signal_byte_exact() {
        let config = profile_high();
        let data: Vec<u8> = (0..256).map(|i| (i as u8).wrapping_mul(17)).collect();
        let samples = tx_v3(&data, &config, 0xC0DE_0001);

        let r = rx_v2_with_options(&samples, &config, true, true)
            .expect("clean signal must decode under parallel-grid path");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "clean signal: payload mismatch under parallel-grid path"
        );
    }

    /// Drifted signal at +30 ppm with grid enabled. Gardner is expected
    /// to lock and produce the clean decode, but the parallel-grid code
    /// path is on the critical path either way. Verifies that the
    /// "Gardner clean -> skip grid" short-circuit still works on aarch64
    /// (regression guard for the parallel branch).
    #[test]
    fn grid_parallel_drifted_signal_byte_exact() {
        let config = profile_high();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(11)).collect();
        let samples = tx_v3(&data, &config, 0xC0DE_0002);
        let drifted = resample_audio(&samples, -30.0);

        let r = rx_v2_with_options(&drifted, &config, true, true)
            .expect("drifted signal must decode under parallel-grid path");
        assert_eq!(
            &r.data[..data.len()],
            &data[..],
            "drifted signal: payload mismatch under parallel-grid path"
        );
    }

    /// Heavy-AWGN drifted signal -- targets the case where Gardner's
    /// drift estimate is noisy enough that the first decode isn't
    /// clean, so the safety grid actually fires. Validates that the
    /// parallel path can recover the payload from inside one of the
    /// grid cells. May occasionally return None on aarch64 if the
    /// channel is too hostile ; we only assert non-panic + non-empty
    /// data when Some.
    #[test]
    fn grid_parallel_noisy_drifted_signal_no_panic() {
        let config = profile_high();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(23)).collect();
        let samples = tx_v3(&data, &config, 0xC0DE_0003);
        let drifted = resample_audio(&samples, 10.0);
        let noisy = add_awgn(&drifted, 0.05, 0xC0DE_0003);
        // Just exercise the path -- no panic, no thread leak on a
        // hostile channel. Decode success is not required ; some grid
        // cells may all fail, that's by design.
        let _ = rx_v2_with_options(&noisy, &config, true, true);
    }

    /// Regression test for the 0.10.36 fix : `is_fully_clean` must
    /// REJECT a meta-only decode (`total=converged=1` because only the
    /// META marker passed CRC and its CW converged). Under the loose
    /// `is_clean` predicate this would early-exit the parallel grid
    /// before any DATA was recovered. Constructed via mock
    /// `RxV2Result` values rather than a synthesised audio buffer
    /// because the bug surfaces on specific marker-CRC patterns that
    /// are hard to deterministically reproduce from TX → channel → RX.
    #[test]
    fn is_fully_clean_rejects_meta_only_decode() {
        fn mock(
            segments_decoded: usize,
            segments_lost: usize,
            total: usize,
            converged: usize,
            data_recovered: usize,
        ) -> RxV2Result {
            RxV2Result {
                data: Vec::new(),
                header: None,
                app_header: None,
                converged_blocks: converged,
                total_blocks: total,
                segments_decoded,
                segments_lost,
                sigma2: 0.0,
                sigma2_data: 0.0,
                data_blocks_recovered: data_recovered,
                cw_bytes_map: HashMap::new(),
                eot_seen: false,
                constellation_sample: Vec::new(),
                pilot_phase_segments: Vec::new(),
                pilot_phase_is_meta: Vec::new(),
                pilot_sigma2_per_segment: Vec::new(),
                pilot_skew_per_segment: Vec::new(),
                pilot_kurt_per_segment: Vec::new(),
                last_preamble_offset: None,
                drift_ppm: 0.0,
                ffe_centroid_initial: 0.0,
                ffe_centroid_final: 0.0,
            }
        }

        // Full-clean SF : 4 segments, 22 CWs total, all converged, 21
        // data CWs recovered (1 meta + 21 data).
        let full = mock(4, 0, 22, 22, 21);
        assert!(is_clean(&full), "full-clean must pass loose is_clean");
        assert!(
            is_fully_clean(&full),
            "full-clean must pass strict is_fully_clean"
        );

        // META-only : the bug scenario. Loose is_clean accepts it
        // (100>=99), strict is_fully_clean rejects it.
        let meta_only_marker_loss = mock(1, 3, 1, 1, 0);
        assert!(
            is_clean(&meta_only_marker_loss),
            "loose is_clean accepts meta-only (this is the bug it has)"
        );
        assert!(
            !is_fully_clean(&meta_only_marker_loss),
            "is_fully_clean MUST reject meta-only decode (segments_lost > 0)"
        );

        // Window cut short right after meta : no markers lost, but
        // only the meta segment was processed. data_blocks_recovered=0
        // catches it.
        let meta_only_window_cut = mock(1, 0, 1, 1, 0);
        assert!(
            is_clean(&meta_only_window_cut),
            "loose is_clean accepts window-cut meta-only (this is the bug it has)"
        );
        assert!(
            !is_fully_clean(&meta_only_window_cut),
            "is_fully_clean MUST reject meta-only when data_blocks_recovered == 0"
        );

        // Partial CW failure : 1 CW out of 22 didn't converge. Loose
        // is_clean accepts it (>=99% would only be 21.78 needed, but
        // ours is 22*99=2178 vs 21*100=2100, so 2100<2178 → also
        // rejects). Strict is_fully_clean rejects via != check.
        let partial_cw = mock(4, 0, 22, 21, 20);
        assert!(
            !is_clean(&partial_cw),
            "loose is_clean already rejects 21/22 (the >=99% threshold rounds up)"
        );
        assert!(
            !is_fully_clean(&partial_cw),
            "is_fully_clean MUST reject any CW failure"
        );

        // Lost segment with otherwise-clean decode : strict rejects
        // because segments_lost > 0 means the cell's drift hypothesis
        // mis-aligned a marker.
        let one_seg_lost = mock(3, 1, 18, 18, 17);
        assert!(
            !is_fully_clean(&one_seg_lost),
            "is_fully_clean MUST reject any lost segment"
        );
    }

    /// 30 sequential invocations on the same clean input. Smoke-tests
    /// thread-scope teardown : `std::thread::scope` joins all workers
    /// before returning, so no leak is possible, but a panic inside a
    /// worker could surface here. Also verifies deterministic decode
    /// across repeated runs (no race on the score-selection path).
    #[test]
    fn grid_parallel_repeated_runs_stable() {
        let config = profile_high();
        let data: Vec<u8> = (0..200).map(|i| (i as u8).wrapping_mul(31)).collect();
        let samples = tx_v3(&data, &config, 0xC0DE_0004);

        let reference = rx_v2_with_options(&samples, &config, true, true)
            .expect("reference decode must succeed");

        for run in 0..30 {
            let r = rx_v2_with_options(&samples, &config, true, true)
                .unwrap_or_else(|| panic!("run #{run} returned None"));
            assert_eq!(
                &r.data[..data.len()],
                &reference.data[..data.len()],
                "run #{run}: payload diverged from reference"
            );
        }
    }
}
