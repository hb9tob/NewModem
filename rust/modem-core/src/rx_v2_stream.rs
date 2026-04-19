//! Incremental v2 stream decoder: O(new audio) per feed.
//!
//! Complements `rx_v2::rx_v2_single` by providing a true stream-decode with
//! every piece of heavy state (FFE taps, DD-PLL phase, marker cursor,
//! session_id lock, per-ESI codeword cache, sigma² accumulators, AppHeader
//! lock) persisted across successive `feed_samples` calls.
//!
//! The caller is responsible for preamble detection: a `StreamingDecoder` is
//! instantiated once the state machine is confident a preamble has been
//! locked onto, and is then fed audio from the preamble onwards. After an
//! internal one-shot initialisation (downmix + MF + sync::find_preamble +
//! FFE LS training + header decode) the per-feed cost is bounded by the feed
//! size, independent of the cumulative transmission duration — which is the
//! whole point of this module.
//!
//! Strictly additive: no existing module is modified. Building blocks from
//! `demodulator`, `ffe`, `sync`, `marker`, `pilot`, `interleaver`,
//! `soft_demod`, `ldpc::decoder`, `preamble`, `header`, `app_header`,
//! `rrc` are composed. The pilot-aided per-segment tracking helper is
//! duplicated here (the rx_v2 version is a private fn).

use std::collections::{HashMap, VecDeque};
use std::f64::consts::PI;

use crate::app_header::{self, AppHeader};
use crate::constellation::Constellation;
use crate::ffe;
use crate::frame::{self, HEADER_VERSION_V2, HEADER_VERSION_V3, V2_CODEWORDS_PER_SEGMENT};
use crate::header::{self, Header};
use crate::interleaver;
use crate::ldpc::decoder::LdpcDecoder;
use crate::pll::DdPll;
use crate::marker::{self, MarkerPayload, MARKER_LEN, MARKER_SYNC_LEN};
use crate::pilot;
use crate::preamble;
use crate::profile::{ModemConfig, ProfileIndex};
use crate::rrc::{self, rrc_taps};
use crate::soft_demod;
use crate::sync;
use crate::types::{Complex64, AUDIO_RATE, D_SYMS, N_PREAMBLE, P_SYMS, RRC_SPAN_SYM};

const HEADER_SYM_COUNT: usize = 96;
const NARROW_WINDOW: usize = 8;
const WIDE_WINDOW: usize = 512;

// Same learning rates as the batch rx_v2_single pipeline.
const MU_TRAIN: f64 = 0.10;
const MU_DD: f64 = 0.02;

// ============================================================================
// Marker-anchored timing recovery
//
// Every segment begins with a 32-symbol CAZAC-like sync pattern whose complex
// values are fixed and known to the receiver. After `find_sync_in_window`
// locates a marker and `decode_marker_at` validates its Golay+CRC8 payload,
// we have a HIGH-CONFIDENCE anchor of known symbols at a known position in
// `syms`. We then correlate the known sync pattern against the underlying
// `fse_input` at three fractional offsets (−1, 0, +1 fse samples) and
// parabolic-fit the peak. That gives a sub-sample timing estimate driven
// entirely by DATA-INDEPENDENT references — no Gardner self-noise, no
// decision-directed failure mode on marginal 16-APSK segments.
//
// The offset feeds a simple first-order low-pass into `timing_phase`, which
// the FFE sampler applies as a fractional sample shift. Each segment gives
// us one fresh measurement; at NORMAL (1500 Bd, ~1.1 s per data segment)
// that's a ~1 Hz loop bandwidth — ample for USB sound-card drift (quasi-
// constant over minutes).
// ============================================================================

/// Proportional gain on the per-marker sub-sample residual. Moderate
/// value: enough to converge within a few markers, low enough to ignore
/// fit noise on short TX where drift has barely accumulated.
const TIMING_UPDATE_ALPHA: f64 = 0.5;

/// Integral gain on the per-symbol drift-rate estimate (LPF blend factor).
const TIMING_UPDATE_BETA: f64 = 0.5;

/// How many consecutive marker-decode failures before we treat ourselves
/// as INTERRUPTED (squelched, stepped-on by voice, broadband parasites).
/// At that point the adaptive loops freeze until a fresh marker validates
/// the stream again. 3 failures covers any short gap but avoids freezing
/// on an isolated CRC/Golay miss.
const INTERRUPTION_FAILS: usize = 3;

/// Safety clamp on the sub-sample offset estimated from a single marker,
/// expressed as a fraction of `pitch_fse` (= 1 fse sample / symbol). The
/// correlation window spans ±pitch_fse/2 = ±½ symbol; anything reported
/// beyond that half-symbol is a parabolic-fit artefact on a flat or
/// multi-modal correlation surface, not a real offset.
const MAX_TIMING_STEP_FRAC: f64 = 0.55;

/// Clamp on `timing_rate_per_sym` in SYMBOLS / symbol (= ppm / 1e6 scaled
/// out of pitch_fse). 1e-3 symbols/sym = 1000 ppm-equivalent, which covers
/// the worst realistic sound-card drift by a wide margin.
const MAX_TIMING_RATE_PER_SYM_SYM: f64 = 1.0e-3;

/// Number of successfully-decoded markers after which we seed
/// `timing_rate_per_sym` from a bulk ppm estimate. The batch `rx_v2::rx_v2`
/// wrapper finds a global resample ppm by grid-search; on captures where the
/// Gardner PI loop would otherwise converge on a biased fixed-point, that
/// one-shot seed lifts LDPC convergence from ~75 % to ~100 %. 5 markers is
/// enough for the cumulative inter-marker span to dominate integer-symbol
/// quantisation noise while keeping the pre-seed window short (~5 s on MEGA,
/// ~5 s on NORMAL).
const BULK_DRIFT_MARKER_COUNT: usize = 5;
/// Sliding-window cap on the marker log used for LS regression. ~50
/// markers = ~30–50 s of history at 1 marker/s. Bounds memory and lets
/// the estimate track slowly-varying drift (TCXO thermal drift, radio
/// PLL re-lock). Set high enough that the regression is noise-dominated
/// (not bias-dominated by the window edge) on stationary drift.
const BULK_DRIFT_LOG_MAX: usize = 50;

/// Maximum recent corrected data symbols kept for GUI constellation
/// display. Balance between visual density (more points = clearer
/// cluster shapes) and JSON event size (each point is 2 × f32 = 8 B).
const RECENT_DATA_SYMS_CAP: usize = 128;
/// Subsample stride when harvesting new segment data-syms into the
/// ring buffer. For MEGA 2-CW segments of 1152 data syms, stride 24
/// yields ~48 samples per segment. Keeps the buffer heterogeneous
/// across segments (vs sampling consecutive data-syms from one seg).
const RECENT_DATA_SYMS_STRIDE: usize = 24;

/// Minimum marker count before the pre-resample bootstrap replay is
/// triggered. Too low (n=5): ppm noise is ±10 ppm, replay commits to a
/// wrong resample factor and the session stays sub-optimal. Too high
/// (n=30): pre-replay audio spends >30 s at degraded quality. n=10
/// balances: regression noise ~±5 ppm which is below the per-segment
/// drift tolerance of 16-APSK.
const PRE_RESAMPLE_MIN_MARKERS: usize = 10;
/// Below this |ppm| estimate, skip the bootstrap replay entirely —
/// running a full reset+redo to correct <1 ppm of drift wastes work
/// and adds resample-interpolation noise on a signal that was already
/// near-nominal.
const PRE_RESAMPLE_MIN_ABS_PPM: f64 = 1.0;

/// Linear resampling of `samples` to compensate a ppm mismatch. Same
/// formula as `rx_v2::resample_audio` (which is module-private): for
/// positive ppm (RX clock faster), output is SHORTER and matches the
/// TX time grid. Duplicated here to keep this file strictly additive
/// (no modifications to rx_v2).
fn resample_audio_f32(samples: &[f32], drift_ppm: f64) -> Vec<f32> {
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

/// Incremental, chunk-driven linear resampler. Tracks the absolute
/// position of the next output sample (`output_cum × ratio` in input
/// coordinates) and the absolute position of the buffered input head
/// (`input_cum`), so chunk-boundary fractional offsets are handled
/// without accumulating rounding drift.
///
/// After the one-shot pre-resample replay, `prime_after_bulk` is called
/// to record that the bulk-resample step already consumed N raw samples
/// and emitted M outputs. Subsequent `consume` calls then pick up
/// exactly where the bulk finished.
#[derive(Debug)]
struct IncrementalResampler {
    ratio: f64,
    /// Total output samples emitted so far (bulk + incremental).
    output_cum: u64,
    /// Absolute input position where `buffer[0]` sits. Advances when
    /// `buffer` is trimmed in `consume`.
    input_cum: u64,
    /// Pending input samples ahead of `input_cum`. Bounded at ~1 chunk
    /// worth: after each `consume`, we drop everything before the
    /// index the next output sample will read.
    buffer: Vec<f32>,
}

impl IncrementalResampler {
    fn new(ppm: f64) -> Self {
        Self {
            ratio: 1.0 + ppm * 1e-6,
            output_cum: 0,
            input_cum: 0,
            buffer: Vec::new(),
        }
    }
    /// Record that a bulk `resample_audio_f32` already consumed
    /// `input_consumed` raw samples and emitted `output_produced`
    /// outputs. The next `consume` call picks up from that point.
    fn prime_after_bulk(&mut self, input_consumed: u64, output_produced: u64) {
        self.output_cum = output_produced;
        self.input_cum = input_consumed;
        self.buffer.clear();
    }
    /// Consume a new raw chunk and return the resampled output for this
    /// chunk. Trims the internal buffer after emission to keep memory
    /// bounded by one chunk worth of lookahead.
    fn consume(&mut self, new_input: &[f32]) -> Vec<f32> {
        self.buffer.extend_from_slice(new_input);
        let mut out = Vec::new();
        loop {
            let abs_pos = self.output_cum as f64 * self.ratio;
            let local = abs_pos - self.input_cum as f64;
            if local < 0.0 {
                // Output position sits BEFORE the buffer origin —
                // impossible with correct priming, skip defensively.
                self.output_cum += 1;
                continue;
            }
            let idx_f = local.floor();
            let idx = idx_f as usize;
            if idx + 1 >= self.buffer.len() {
                break;
            }
            let frac = (local - idx_f) as f32;
            out.push((1.0 - frac) * self.buffer[idx] + frac * self.buffer[idx + 1]);
            self.output_cum += 1;
        }
        // Trim: drop all samples strictly before the next output's
        // read index (minus one, to keep a single sample of interp
        // context across the boundary).
        let next_abs = self.output_cum as f64 * self.ratio;
        let next_local = (next_abs - self.input_cum as f64).floor().max(0.0) as usize;
        let drop = next_local.saturating_sub(1);
        if drop > 0 && drop <= self.buffer.len() {
            self.buffer.drain(0..drop);
            self.input_cum += drop as u64;
        }
        out
    }
}

/// Live streaming decoder with per-feed incremental state.
pub struct StreamingDecoder {
    // --- Immutable configuration --------------------------------------------
    config: ModemConfig,
    taps: Vec<f64>,
    mf_half: usize,
    sps: usize,
    pitch: usize,
    bps: usize,
    syms_per_cw: usize,
    k_bytes: usize,
    constellation: Constellation,
    preamble_syms: Vec<Complex64>,
    deinterleave_perm: Vec<usize>,
    decoder: LdpcDecoder,

    // --- Audio state (grows linearly) ---------------------------------------
    samples_processed: u64,
    bb: Vec<Complex64>,
    mf: Vec<Complex64>,

    // --- FFE init state (filled once) ---------------------------------------
    ffe_initialized: bool,
    init_failed: bool,
    d_fse: usize,
    pitch_fse: usize,
    sps_fse: usize,
    n_ff: usize,
    ffe_half: usize,
    fse_decim_start: usize,
    fse_start: usize,
    fse_input: Vec<Complex64>,
    ffe_taps: Vec<Complex64>,
    gain: Complex64,
    header: Option<Header>,

    // --- Symbol stream (grows one symbol at a time) -------------------------
    next_sym_k: usize,
    syms: Vec<Complex64>,

    // --- Marker walker state ------------------------------------------------
    cursor: usize,
    session_id_low_lock: Option<u8>,
    consecutive_fails: usize,
    walker_stopped: bool,
    app_hdr: Option<AppHeader>,
    cw_bytes: HashMap<u32, Vec<u8>>,
    total_blocks: usize,
    converged_blocks: usize,
    segments_decoded: usize,
    segments_lost: usize,
    sigma2_sum: f64,
    sigma2_count: usize,

    // New-marker queue: caller drains this for `MarkerSynced` events.
    new_markers: Vec<MarkerPayload>,

    // --- Timing-recovery state ---------------------------------------------
    /// Cumulative fractional-sample offset the FFE sampler applies to its
    /// symbol centres. Starts at 0 (preamble training aligns us at t=0) and
    /// is nudged each time a marker sync pattern is correlated sub-sample,
    /// plus ramps smoothly between markers at `timing_rate_per_sym`.
    timing_phase: f64,
    /// Smoothly-applied per-symbol ramp. Updated at each marker from the
    /// observed offset accumulated since the previous marker; carried
    /// forward between markers so the FFE DD-LMS sees a continuous slide
    /// rather than step jumps (which would destabilise the taps on heavy
    /// drift / long inter-marker intervals like ULTRA's 5-second segs).
    timing_rate_per_sym: f64,
    /// Symbol index of the last marker that drove a timing update.
    last_marker_sym_k: Option<usize>,

    // --- Interruption / recovery state ------------------------------------
    /// Tracks whether the walker was in an "interrupted" regime on the
    /// previous symbol (i.e. `consecutive_fails >= INTERRUPTION_FAILS`).
    /// Flipped back to false when a fresh marker clears the fail counter;
    /// the flip itself arms `realign_on_next_marker`.
    interrupted: bool,
    /// Cursor position right after the last successfully decoded marker.
    /// On entering an interruption we rewind `self.cursor` to this anchor
    /// so forward scanning resumes from a known-good point instead of
    /// wherever the MARKER_LEN-per-fail walk happened to leave it (which
    /// can easily drift past the first real post-gap marker).
    last_good_cursor: usize,
    /// Set to true when we transition out of an interruption, so the
    /// next marker decoded after it hard-realigns `timing_phase` against
    /// its known sync pattern instead of the usual smoothed α-correction.
    realign_on_next_marker: bool,

    // --- Bulk drift seed (one-shot) ----------------------------------------
    /// Feature flag: when true, the first `BULK_DRIFT_MARKER_COUNT` validated
    /// markers are used to estimate a global TX↔RX ppm drift (same formula
    /// as `rx_v2::estimate_drift_ppm`), and the result overrides
    /// `timing_rate_per_sym` once. Gardner PI then refines from that seed.
    /// Disabled paths preserve the pre-existing pure-Gardner behaviour for
    /// A/B comparisons and test fidelity.
    enable_bulk_drift_seed: bool,
    /// Set to true once the one-shot seed has been applied, so the mechanism
    /// never triggers twice in a session.
    bulk_drift_seeded: bool,
    /// (fse_pos_at_marker_center, is_meta) for each of the first few
    /// validated markers, in arrival order. `fse_pos` is the fractional
    /// position in `fse_input` of the marker's first symbol center,
    /// including the live `timing_phase` correction that Gardner PI has
    /// already applied — i.e. it's the *true* sub-sample position in
    /// the audio stream, not the symbolic index. Measuring the drift
    /// from this quantity makes the estimate orthogonal to whatever
    /// Gardner is doing (the symbolic index alone is made invariant by
    /// Gardner's phase tracking and therefore carries no drift info).
    bulk_drift_marker_log: Vec<(f64, bool)>,

    // --- DD-PLL residual phase tracking -----------------------------------
    /// Decision-directed PLL applied per-data-symbol AFTER `track_segment`'s
    /// pilot-interpolation correction. Pilots catch slow/segment-scale
    /// phase drift (34-symbol cadence); the PLL catches residual fast
    /// phase noise that would otherwise smear the constellation clusters.
    /// State persists across segments so the loop tracks carrier-frequency
    /// offset continuously (same role as `modem_apsk16_ftn_bench.py`'s
    /// DD-PLL 2e ordre, gains scaled for BW ≈ 1 % Rs, critically damped).
    dd_pll: DdPll,

    // --- GUI diagnostic buffers -------------------------------------------
    /// Ring buffer of the most-recent corrected data symbols seen in
    /// `walk_markers` (up to `RECENT_DATA_SYMS_CAP` entries). Used by the
    /// GUI to render a live constellation of the main-modulation symbols
    /// without re-running the decode pipeline.
    recent_data_syms: VecDeque<Complex64>,

    // --- Pre-resample bootstrap state -------------------------------------
    /// Last ppm estimate produced by `try_seed_bulk_drift`. Used by the
    /// bootstrap replay trigger to decide whether a reset+redo is
    /// worth it.
    last_ppm_estimate: f64,
    /// Running copy of all raw input samples since session start. Only
    /// retained until the bootstrap replay fires (or forever, if it
    /// never does); dropped/cleared afterwards. Bounded in practice by
    /// the few seconds it takes for `PRE_RESAMPLE_MIN_MARKERS` markers
    /// to accumulate (~10 s = 1.9 MB at 48 kHz f32), not by the full
    /// session length.
    raw_samples_for_replay: Vec<f32>,
    /// Once the bootstrap replay has fired, this holds the incremental
    /// resampler used to convert every future raw chunk to the TX-time
    /// grid before it enters `append_bb`. Drift from that point on is
    /// absorbed at the audio level (same strategy as batch `rx_v2`'s
    /// grid-search pre-resample), not tracked via `timing_rate_per_sym`.
    pre_resample_state: Option<IncrementalResampler>,
    /// Locked-in ppm value the incremental resampler uses. Purely for
    /// diagnostics / events.
    pre_resample_ppm: f64,
}

impl StreamingDecoder {
    /// Build a fresh decoder for the given profile. Call `feed_samples` with
    /// audio starting at (or just before) the preamble — any prefix before
    /// the preamble is tolerated because preamble detection is re-run
    /// internally, but feeding hundreds of seconds of pre-preamble silence
    /// wastes work.
    pub fn new(profile: ProfileIndex) -> Self {
        let config = profile.to_config();
        let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
            .expect("invalid profile");
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let mf_half = taps.len() / 2;
        let constellation = frame::make_constellation(&config);
        let bps = config.constellation.bits_per_sym();
        let decoder = LdpcDecoder::new(config.ldpc_rate, 50);
        let deinterleave_perm = interleaver::deinterleave_table(decoder.n(), config.constellation);
        let syms_per_cw = decoder.n() / bps;
        let k_bytes = decoder.k() / 8;
        let preamble_syms = preamble::make_preamble();

        Self {
            config,
            taps,
            mf_half,
            sps,
            pitch,
            bps,
            syms_per_cw,
            k_bytes,
            constellation,
            preamble_syms,
            deinterleave_perm,
            decoder,

            samples_processed: 0,
            bb: Vec::new(),
            mf: Vec::new(),

            ffe_initialized: false,
            init_failed: false,
            d_fse: 0,
            pitch_fse: 0,
            sps_fse: 0,
            n_ff: 0,
            ffe_half: 0,
            fse_decim_start: 0,
            fse_start: 0,
            fse_input: Vec::new(),
            ffe_taps: Vec::new(),
            gain: Complex64::new(1.0, 0.0),
            header: None,

            next_sym_k: 0,
            syms: Vec::new(),

            cursor: 0,
            session_id_low_lock: None,
            consecutive_fails: 0,
            walker_stopped: false,
            app_hdr: None,
            cw_bytes: HashMap::new(),
            total_blocks: 0,
            converged_blocks: 0,
            segments_decoded: 0,
            segments_lost: 0,
            sigma2_sum: 0.0,
            sigma2_count: 0,

            new_markers: Vec::new(),

            timing_phase: 0.0,
            timing_rate_per_sym: 0.0,
            last_marker_sym_k: None,
            interrupted: false,
            last_good_cursor: 0,
            realign_on_next_marker: false,

            enable_bulk_drift_seed: true,
            bulk_drift_seeded: false,
            bulk_drift_marker_log: Vec::with_capacity(BULK_DRIFT_MARKER_COUNT),

            dd_pll: {
                // Same loop gains as batch rx_v2: α=0.05, β=α²/4 ≈ 0.000625.
                // Critically-damped 2nd-order PI, BW ≈ 1 % Rs.
                let alpha = 0.05f64;
                let beta = alpha * alpha * 0.25;
                DdPll::new(alpha, beta)
            },

            recent_data_syms: VecDeque::with_capacity(RECENT_DATA_SYMS_CAP),

            last_ppm_estimate: 0.0,
            raw_samples_for_replay: Vec::new(),
            pre_resample_state: None,
            pre_resample_ppm: 0.0,
        }
    }

    /// Enable or disable the one-shot bulk-drift ppm seed mechanism. Enabled
    /// by default. Intended for A/B comparisons and regression tests that
    /// need to exercise the pure Gardner PI path.
    pub fn set_bulk_drift_seed_enabled(&mut self, enabled: bool) {
        self.enable_bulk_drift_seed = enabled;
    }

    /// Returns whether the one-shot seed has already been applied in this
    /// session. Exposed for diagnostic logging.
    pub fn bulk_drift_seeded(&self) -> bool {
        self.bulk_drift_seeded
    }

    /// Current cumulative timing-phase correction applied to the FFE
    /// sampler, expressed in fse_input samples. Should stay close to zero
    /// on drift-free input and grow monotonically (slope ≈ drift ppm ×
    /// pitch_fse × fs / 1e6) on drifted input.
    pub fn timing_phase(&self) -> f64 {
        self.timing_phase
    }

    /// Ingest a chunk of raw f32 audio. Work done per call is proportional to
    /// the chunk size plus the number of newly-completed LDPC codewords. All
    /// heavy state is retained between calls — feeding the same cumulative
    /// audio in many small batches is equivalent to feeding it as one batch
    /// (modulo tiny DD-LMS convergence differences).
    pub fn feed_samples(&mut self, samples: &[f32]) {
        if self.init_failed {
            return;
        }
        // Pre-bootstrap: buffer raw samples so the bootstrap replay can
        // reprocess the pre-replay audio at the correct ppm. After the
        // replay fires, raw_samples_for_replay is cleared and incoming
        // chunks are resampled live via the IncrementalResampler.
        let resampled_buf: Vec<f32>;
        let to_process: &[f32] = if let Some(state) = self.pre_resample_state.as_mut() {
            resampled_buf = state.consume(samples);
            &resampled_buf
        } else {
            self.raw_samples_for_replay.extend_from_slice(samples);
            samples
        };
        self.append_bb(to_process);
        self.extend_mf();
        if !self.ffe_initialized {
            if !self.try_initialize() {
                return;
            }
        }
        self.extend_fse();
        self.extend_syms();
        self.walk_markers();
        self.maybe_trigger_pre_resample_replay();
    }

    pub fn is_initialized(&self) -> bool {
        self.ffe_initialized
    }

    pub fn init_failed(&self) -> bool {
        self.init_failed
    }

    pub fn header(&self) -> Option<&Header> {
        self.header.as_ref()
    }

    pub fn app_header(&self) -> Option<&AppHeader> {
        self.app_hdr.as_ref()
    }

    pub fn data_blocks_recovered(&self) -> usize {
        self.cw_bytes.len()
    }

    /// Bitmap of which data ESIs have converged, indexed 0..n_expected.
    /// Bit `i` (counted LSB-first within byte `i/8`) is 1 iff the data
    /// codeword at ESI `i` has been decoded and LDPC-converged. Used by
    /// the GUI to paint the per-block progress bar.
    pub fn converged_esi_bitmap(&self, n_expected: usize) -> Vec<u8> {
        let n_bytes = (n_expected + 7) / 8;
        let mut out = vec![0u8; n_bytes];
        for &esi in self.cw_bytes.keys() {
            let i = esi as usize;
            if i < n_expected {
                out[i / 8] |= 1 << (i % 8);
            }
        }
        out
    }

    /// Recent corrected data symbols (up to the ring-buffer cap, currently
    /// ~128), as flat `[re, im]` pairs for easy JSON/event emission to the
    /// frontend constellation display.
    pub fn recent_data_syms(&self) -> Vec<[f32; 2]> {
        self.recent_data_syms
            .iter()
            .map(|c| [c.re as f32, c.im as f32])
            .collect()
    }

    pub fn total_blocks(&self) -> usize {
        self.total_blocks
    }

    pub fn converged_blocks(&self) -> usize {
        self.converged_blocks
    }

    pub fn sigma2(&self) -> f64 {
        if self.sigma2_count > 0 {
            (self.sigma2_sum / self.sigma2_count as f64).max(1e-6)
        } else {
            1.0
        }
    }

    pub fn segments_decoded(&self) -> usize {
        self.segments_decoded
    }

    pub fn segments_lost(&self) -> usize {
        self.segments_lost
    }

    /// Drain the queue of markers decoded since the previous `drain_new_markers`
    /// call. Used by the outer state machine to emit `MarkerSynced` events.
    pub fn drain_new_markers(&mut self) -> Vec<MarkerPayload> {
        std::mem::take(&mut self.new_markers)
    }

    /// Number of data codewords the TX would emit for the currently-known
    /// AppHeader. 0 if the AppHeader hasn't been decoded yet.
    pub fn expected_data_blocks(&self) -> usize {
        match self.app_hdr {
            Some(ref ah) => {
                ((ah.file_size as usize) + self.k_bytes - 1) / self.k_bytes
            }
            None => 0,
        }
    }

    /// Assemble the current payload from `cw_bytes` in ESI order, truncated to
    /// the AppHeader's `file_size`. Missing ESIs are zero-padded. Returns
    /// `None` if the AppHeader hasn't been recovered (can't know file_size).
    pub fn try_assemble(&self) -> Option<Vec<u8>> {
        let ah = self.app_hdr.as_ref()?;
        let n_source_cw = self.expected_data_blocks();
        let mut out = Vec::with_capacity(n_source_cw * self.k_bytes);
        for esi in 0..n_source_cw as u32 {
            match self.cw_bytes.get(&esi) {
                Some(bytes) => out.extend_from_slice(bytes),
                None => out.extend(std::iter::repeat(0u8).take(self.k_bytes)),
            }
        }
        out.truncate(ah.file_size as usize);
        Some(out)
    }

    /// Number of data ESIs that would round-trip the full payload.
    pub fn all_data_present(&self) -> bool {
        let expected = self.expected_data_blocks();
        expected > 0 && self.cw_bytes.len() >= expected
    }

    // ========================================================================
    // Private incremental stages
    // ========================================================================

    fn append_bb(&mut self, samples: &[f32]) {
        // Absolute-index-phase downmix: matches `demodulator::downmix`'s phase
        // convention except that the origin is wherever StreamingDecoder
        // started (i.e. not the TX's sample 0). The resulting constant carrier
        // phase offset is absorbed by the global-gain LS on the preamble.
        let fc = self.config.center_freq_hz;
        let sr = AUDIO_RATE as f64;
        let start = self.samples_processed;
        self.bb.reserve(samples.len());
        for (i, &s) in samples.iter().enumerate() {
            let abs_i = start + i as u64;
            let phase = -2.0 * PI * fc * abs_i as f64 / sr;
            let carrier = Complex64::new(phase.cos(), phase.sin());
            self.bb.push(carrier * s as f64);
        }
        self.samples_processed += samples.len() as u64;
    }

    fn extend_mf(&mut self) {
        let m = self.taps.len();
        let half = self.mf_half;
        // mf[i] uses bb[i - half .. i - half + m]. Committed when the upper
        // bound is in range: i + half + 1 <= bb.len()  ⇔  i < bb.len() - half.
        // For i < half we still commit using zero-pad on the front — same
        // behaviour as `demodulator::matched_filter`, which is "same"-mode.
        let target = self.bb.len().saturating_sub(half);
        if self.mf.len() >= target {
            return;
        }
        self.mf.reserve(target - self.mf.len());
        while self.mf.len() < target {
            let i = self.mf.len();
            let mut acc = Complex64::new(0.0, 0.0);
            let j_base = i as isize - half as isize;
            for k in 0..m {
                let j = j_base + k as isize;
                if j >= 0 && (j as usize) < self.bb.len() {
                    acc += self.bb[j as usize] * self.taps[k];
                }
            }
            self.mf.push(acc);
        }
    }

    /// One-shot initialisation: find the preamble inside `self.mf`, compute
    /// decimation parameters, LS-train the FFE on the preamble, apply the
    /// FFE LMS for the preamble + protocol-header window, recover the global
    /// gain from the preamble, and decode the protocol header. Returns true
    /// on success; false if we don't yet have enough audio.
    fn try_initialize(&mut self) -> bool {
        // Need mf to cover preamble + header + some margin before trying.
        let min_mf_for_preamble = (N_PREAMBLE + HEADER_SYM_COUNT + 16) * self.pitch;
        if self.mf.len() < min_mf_for_preamble {
            return false;
        }

        let sync_pos = match sync::find_preamble(&self.mf, self.sps, self.pitch, self.config.beta) {
            Some(p) => p,
            None => return false,
        };

        let (fse_initial, fse_start, d_fse) =
            sync::decimate_for_fse(&self.mf, sync_pos, self.sps, self.pitch);
        self.d_fse = d_fse;
        self.pitch_fse = self.pitch / d_fse;
        self.sps_fse = self.sps / d_fse;
        let tau_eff = self.pitch_fse as f64 / self.sps_fse as f64;
        let mut n_ff = if tau_eff >= 0.99 {
            8 * self.sps_fse + 1
        } else {
            4 * self.sps_fse + 1
        };
        if n_ff % 2 == 0 {
            n_ff += 1;
        }
        self.n_ff = n_ff;
        self.ffe_half = n_ff / 2;
        // fse_decim_start = first mf index used by fse_input[0]
        //                 = sync_pos - fse_start * d_fse
        self.fse_decim_start = sync_pos - fse_start * d_fse;
        self.fse_start = fse_start;
        self.fse_input = fse_initial;

        // FFE training requires fse_input long enough for the last symbol of
        // the preamble + header window.
        let required_syms = N_PREAMBLE + HEADER_SYM_COUNT;
        let last_center = self.fse_start + (required_syms - 1) * self.pitch_fse;
        if last_center + self.ffe_half + 1 > self.fse_input.len() {
            // Not enough decimated samples yet. Reset partial init so we retry
            // next feed without persistent state (cheap: find_preamble is the
            // only heavy call we've done so far).
            self.fse_input.clear();
            return false;
        }

        // LS-train on preamble positions.
        let training_positions: Vec<usize> = (0..N_PREAMBLE)
            .map(|k| self.fse_start + k * self.pitch_fse)
            .collect();
        let ffe_initial =
            ffe::train_ffe_ls(&self.fse_input, &self.preamble_syms, &training_positions, n_ff);

        // Apply FFE LMS for preamble + header symbols (training on preamble,
        // slicer on header). This sets up the taps exactly as the batch
        // pipeline does — the per-symbol `extend_syms` loop picks up from
        // `next_sym_k = required_syms` with these taps as its starting point.
        let preamble_training: Vec<(usize, Complex64)> = self
            .preamble_syms
            .iter()
            .enumerate()
            .map(|(k, &s)| (k, s))
            .collect();
        let (outputs, final_taps) = ffe::apply_ffe_lms_with_training(
            &self.fse_input,
            &ffe_initial,
            self.fse_start,
            self.pitch_fse,
            required_syms,
            &preamble_training,
            &self.constellation,
            MU_TRAIN,
            MU_DD,
        );
        self.ffe_taps = final_taps;
        self.timing_phase = 0.0;
        self.timing_rate_per_sym = 0.0;
        self.last_marker_sym_k = None;

        // Global gain from preamble outputs vs known preamble symbols.
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..N_PREAMBLE {
            num += outputs[k] * self.preamble_syms[k].conj();
            den += self.preamble_syms[k].norm_sqr();
        }
        let gain = if den > 1e-12 {
            num / den
        } else {
            Complex64::new(1.0, 0.0)
        };
        self.gain = gain;

        // Decode protocol header on the gain-corrected subrange.
        let header_syms: Vec<Complex64> = outputs[N_PREAMBLE..N_PREAMBLE + HEADER_SYM_COUNT]
            .iter()
            .map(|&s| s / gain)
            .collect();
        let decoded_header = match header::decode_header_symbols(&header_syms) {
            Some(h) if h.version == HEADER_VERSION_V2 || h.version == HEADER_VERSION_V3 => h,
            _ => {
                // Preamble locked but header didn't decode as v2/v3 — treat
                // as a permanent init failure. The outer state machine's
                // finalise path (rx_v2 batch fallback) will have a chance to
                // recover.
                self.init_failed = true;
                return false;
            }
        };
        self.header = Some(decoded_header);

        // Commit gain-corrected outputs to the syms buffer and set next_sym_k.
        self.syms.reserve(required_syms);
        for &y in &outputs {
            self.syms.push(y / gain);
        }
        self.next_sym_k = required_syms;
        self.ffe_initialized = true;
        true
    }

    fn extend_fse(&mut self) {
        let d = self.d_fse;
        let start = self.fse_decim_start;
        loop {
            let j = self.fse_input.len();
            let mf_idx = start + j * d;
            if mf_idx >= self.mf.len() {
                break;
            }
            self.fse_input.push(self.mf[mf_idx]);
        }
    }

    fn extend_syms(&mut self) {
        let n_ff = self.n_ff;
        let half = self.ffe_half;
        let pitch_fse = self.pitch_fse;
        loop {
            let k = self.next_sym_k;
            let target_center = self.fse_start as f64
                + (k * pitch_fse) as f64
                + self.timing_phase;
            let center_int = target_center.floor() as isize;
            let frac = target_center - center_int as f64;
            if center_int < half as isize {
                // Would underflow the fse_input left edge.
                break;
            }
            let center_int = center_int as usize;
            // Need both FFE(center_int) and FFE(center_int + 1) for the
            // fractional interpolation, so the right-edge bound is one
            // sample further out than the pure-integer case.
            if center_int + half + 2 > self.fse_input.len() {
                break;
            }
            let lo = center_int - half;
            let mut y0 = Complex64::new(0.0, 0.0);
            let mut y1 = Complex64::new(0.0, 0.0);
            for i in 0..n_ff {
                y0 += self.ffe_taps[i] * self.fse_input[lo + i];
                y1 += self.ffe_taps[i] * self.fse_input[lo + 1 + i];
            }
            let y = y0 * (1.0 - frac) + y1 * frac;

            // Per-symbol energy check AND the walker's coarse-grained
            // `interrupted` flag together form the freeze gate. The per-
            // symbol test catches silence/dropouts shorter than one feed
            // (the walker needs at least a few failed scans to flip into
            // `interrupted`); the walker flag catches cases where the
            // signal IS present but is not our modem (voice stepping on
            // us, broadband parasites), where per-symbol energy is not a
            // reliable indicator.
            let mut r_pow = 1e-12f64;
            for i in 0..n_ff {
                let x = self.fse_input[lo + i] * (1.0 - frac)
                    + self.fse_input[lo + 1 + i] * frac;
                r_pow += x.norm_sqr();
            }
            // Threshold just under a typical signal level: unit-power
            // constellations through RRC matched-filter give r_pow ~= n_ff
            // × 1 (≈ 17 for our profiles). Anything two orders below that
            // is either silence or a RRC tail transient into/out of a
            // gap, where NLMS's 1/r_pow blows the update up and the taps
            // diverge. Freeze there.
            let sym_signal = r_pow >= 0.1;
            // FFE DD-LMS only after the bulk-drift seed has produced a
            // rate estimate. Before that, timing_phase is at 0 while drift
            // has accumulated up to ~1.5 fse; 16-APSK decisions at that
            // sampling offset are frequently wrong, and the gradient
            // update learns degraded taps that take many markers to
            // unlearn. `bulk_drift_seeded` flips true once the first LS
            // regression has run (N ≥ BULK_DRIFT_MARKER_COUNT markers).
            //
            // Note: the Python reference bench freezes FFE after LS
            // training (mu_ff_dd=0). Empirically, disabling DD-LMS here
            // wrecks MEGA OTA decoding (convergence drops 95 %→5 %),
            // because the NBFM voice-mode channel has enough slow
            // response variation over 3 min that static taps can't track.
            // Python ran on cleaner simulated channels — not directly
            // comparable.
            let adaptive_ok =
                sym_signal && !self.interrupted && self.bulk_drift_seeded;

            // DD-LMS FFE adaptation on data is necessary ONLY for FTN
            // profiles (τ<1 — currently only MEGA with τ=30/32). For
            // Nyquist (τ=1) profiles HIGH/NORMAL/ROBUST/ULTRA, the LS-
            // trained FFE on the preamble is sufficient and DD-LMS would
            // only introduce decision noise into the taps. Matches the
            // Python reference bench behaviour (mu_ff_dd=0) for non-FTN.
            let ffe_dd_lms_enabled = self.pitch_fse < self.sps_fse;
            if adaptive_ok && ffe_dd_lms_enabled {
                let idx = self.constellation.slice_nearest(&[y])[0];
                let d = self.constellation.points[idx];
                let e = d - y;
                let mu_eff = MU_DD / r_pow;
                for i in 0..n_ff {
                    let x = self.fse_input[lo + i] * (1.0 - frac)
                        + self.fse_input[lo + 1 + i] * frac;
                    self.ffe_taps[i] += Complex64::new(mu_eff, 0.0) * e * x.conj();
                }
            }

            self.syms.push(y / self.gain);
            self.next_sym_k += 1;

            if adaptive_ok && self.last_marker_sym_k.is_some() {
                self.timing_phase += self.timing_rate_per_sym;
            }
        }
    }

    /// Sub-sample offset of a marker's sync pattern against `fse_input`,
    /// via the same 3-point parabolic correlation that Gardner PI uses.
    /// PURE MEASUREMENT — unlike `refine_timing_from_marker` this does not
    /// update `timing_phase` or `timing_rate_per_sym`. Returns `None` if
    /// the parabolic fit degenerates or the correlation window falls
    /// outside `fse_input`.
    fn measure_marker_sub_sample_offset(&self, center_pos: f64) -> Option<f64> {
        let sync = marker::make_sync_pattern();
        let h: f64 = 1.0;
        let corr_mag = |delta: f64| -> Option<f64> {
            let mut sum = Complex64::new(0.0, 0.0);
            for k in 0..MARKER_SYNC_LEN {
                let pos = center_pos + (k * self.pitch_fse) as f64 + delta;
                let y = interp_fse(&self.fse_input, pos)?;
                sum += y * sync[k].conj();
            }
            Some(sum.norm())
        };
        let c_minus = corr_mag(-h)?;
        let c_zero = corr_mag(0.0)?;
        let c_plus = corr_mag(h)?;
        let denom = c_minus - 2.0 * c_zero + c_plus;
        if denom.abs() < 1e-9 {
            return None;
        }
        let delta_opt = h * (c_minus - c_plus) / (2.0 * denom);
        let max_step = MAX_TIMING_STEP_FRAC * self.pitch_fse as f64;
        if !delta_opt.is_finite() || delta_opt.abs() > max_step {
            return None;
        }
        Some(delta_opt)
    }

    /// Continuous drift estimate via marker-chain regression.
    ///
    /// Called after each validated marker is logged. Uses all markers seen
    /// so far (N ≥ `BULK_DRIFT_MARKER_COUNT`) to estimate the TX↔RX ppm via
    /// ordinary least-squares fit of `fse_pos[i] = slope × cum_syms[i] +
    /// offset`. The slope gives `pitch_fse × (1 + ppm×1e-6)`; we override
    /// `timing_rate_per_sym` with the derived rate at every call.
    ///
    /// Rationale (v2): a one-shot estimate over 5 markers has ±10 ppm noise
    /// from sub-sample parabolic-fit jitter on the endpoints. Refreshing the
    /// estimate as the marker log grows reduces noise as ~1/sqrt(N); by 20
    /// markers it's ±2.5 ppm, well within the channel tolerance of 16-APSK.
    /// Per-marker cost is O(N) summations, negligible at ~1 marker/second.
    fn try_seed_bulk_drift(&mut self) {
        if !self.enable_bulk_drift_seed
            || self.bulk_drift_marker_log.len() < BULK_DRIFT_MARKER_COUNT
        {
            return;
        }
        // Cumulative expected-symbol count per marker (0 at marker[0], accumulates
        // each leg size through marker[n-1]). Paired with actual fse_pos for LS.
        let n = self.bulk_drift_marker_log.len();
        let mut cum_syms: Vec<f64> = Vec::with_capacity(n);
        cum_syms.push(0.0);
        let mut acc: f64 = 0.0;
        for i in 0..n - 1 {
            let (_, is_meta) = self.bulk_drift_marker_log[i];
            let n_cw = if is_meta { 1 } else { V2_CODEWORDS_PER_SEGMENT };
            let data_sym_count = n_cw * self.syms_per_cw;
            let n_pilot_groups = (data_sym_count + D_SYMS - 1) / D_SYMS;
            acc += (MARKER_LEN + data_sym_count + n_pilot_groups * P_SYMS) as f64;
            cum_syms.push(acc);
        }
        // Ordinary least-squares slope: a = Σ((x-x̄)(y-ȳ)) / Σ(x-x̄)²
        let fse_y: Vec<f64> = self
            .bulk_drift_marker_log
            .iter()
            .map(|(p, _)| *p)
            .collect();
        let x_mean: f64 = cum_syms.iter().sum::<f64>() / n as f64;
        let y_mean: f64 = fse_y.iter().sum::<f64>() / n as f64;
        let mut num = 0.0f64;
        let mut den = 0.0f64;
        for i in 0..n {
            let dx = cum_syms[i] - x_mean;
            num += dx * (fse_y[i] - y_mean);
            den += dx * dx;
        }
        if den <= 0.0 {
            return;
        }
        let slope = num / den;
        // slope = pitch_fse × (1 + ppm × 1e-6) → ppm = (slope/pitch_fse - 1) × 1e6
        let pitch_f = self.pitch_fse as f64;
        let ppm = (slope / pitch_f - 1.0) * 1e6;
        if !ppm.is_finite() {
            return;
        }
        let rate_fse_per_sym = ppm * 1e-6 * pitch_f;
        let max_rate = MAX_TIMING_RATE_PER_SYM_SYM * pitch_f;
        self.timing_rate_per_sym = rate_fse_per_sym.clamp(-max_rate, max_rate);
        self.bulk_drift_seeded = true;
        self.last_ppm_estimate = ppm;
    }

    /// At bootstrap (LS regression has locked onto a stable ppm), reset
    /// the decoder state and replay all buffered raw audio through a
    /// pre-resample step that flattens the TX↔RX clock drift. From then
    /// on, every incoming raw chunk is resampled live by the
    /// `IncrementalResampler` before entering the downmix → MF → FSE
    /// pipeline, so symbol-rate timing sees a drift-free signal (like
    /// batch `rx_v2`, which grid-searches ppm and resamples the whole
    /// audio once before running its single-pass decoder).
    ///
    /// This is the streaming counterpart of batch's `rx_v2` grid-search
    /// resample, applied once the marker-chain regression has accumulated
    /// enough observations (`PRE_RESAMPLE_MIN_MARKERS`) for its ppm
    /// estimate to be better than the interpolation noise floor.
    fn maybe_trigger_pre_resample_replay(&mut self) {
        if self.pre_resample_state.is_some() {
            return;
        }
        if !self.bulk_drift_seeded {
            return;
        }
        if self.bulk_drift_marker_log.len() < PRE_RESAMPLE_MIN_MARKERS {
            return;
        }
        let ppm = self.last_ppm_estimate;
        if !ppm.is_finite() || ppm.abs() < PRE_RESAMPLE_MIN_ABS_PPM {
            // Drift is small enough that the interpolation noise from
            // resampling would outweigh the drift itself. Keep running
            // with the timing_rate_per_sym ramp. Mark resample as
            // "committed at 0 ppm" so we don't keep retrying.
            self.pre_resample_state = Some(IncrementalResampler::new(0.0));
            self.pre_resample_ppm = 0.0;
            self.raw_samples_for_replay.clear();
            self.raw_samples_for_replay.shrink_to_fit();
            return;
        }
        self.do_pre_resample_replay(ppm);
    }

    /// Reset every per-session state field to its initial value and
    /// replay `raw_samples_for_replay` through the full pipeline after
    /// applying a single bulk resample at `ppm`. Post-replay, installs
    /// an `IncrementalResampler` so future feeds stay on the TX grid.
    fn do_pre_resample_replay(&mut self, ppm: f64) {
        let raw = std::mem::take(&mut self.raw_samples_for_replay);
        let resampled = resample_audio_f32(&raw, ppm);
        let resampled_len = resampled.len() as u64;
        let raw_len = raw.len() as u64;

        // Install the resample state FIRST so the in-replay feeds (which
        // run append_bb/walk_markers below) don't re-trigger this
        // function. The incremental resampler is primed to pick up
        // exactly where the bulk resample left off.
        let mut state = IncrementalResampler::new(ppm);
        state.prime_after_bulk(raw_len, resampled_len);
        self.pre_resample_state = Some(state);
        self.pre_resample_ppm = ppm;

        // Reset every per-session processing field. Keep config-level
        // fields (taps, constellation, decoder, preamble_syms, etc).
        self.reset_processing_state();

        // Replay the bulk-resampled audio through the pipeline as one
        // big feed. `resampled` is already pre-corrected, so it bypasses
        // the incremental resampler and goes straight into append_bb.
        self.append_bb(&resampled);
        self.extend_mf();
        if !self.ffe_initialized {
            if !self.try_initialize() {
                return;
            }
        }
        self.extend_fse();
        self.extend_syms();
        self.walk_markers();
    }

    /// Clear every per-session field, keeping static configuration
    /// (constellation, RRC taps, LDPC decoder, etc). Used when the
    /// pre-resample replay restarts processing from scratch on a
    /// drift-corrected audio buffer.
    fn reset_processing_state(&mut self) {
        self.samples_processed = 0;
        self.bb.clear();
        self.mf.clear();
        self.ffe_initialized = false;
        self.init_failed = false;
        self.d_fse = 0;
        self.pitch_fse = 0;
        self.sps_fse = 0;
        self.n_ff = 0;
        self.ffe_half = 0;
        self.fse_decim_start = 0;
        self.fse_start = 0;
        self.fse_input.clear();
        self.ffe_taps.clear();
        self.gain = Complex64::new(1.0, 0.0);
        self.header = None;
        self.next_sym_k = 0;
        self.syms.clear();
        self.cursor = 0;
        self.session_id_low_lock = None;
        self.consecutive_fails = 0;
        self.walker_stopped = false;
        self.app_hdr = None;
        self.cw_bytes.clear();
        self.total_blocks = 0;
        self.converged_blocks = 0;
        self.segments_decoded = 0;
        self.segments_lost = 0;
        self.sigma2_sum = 0.0;
        self.sigma2_count = 0;
        self.new_markers.clear();
        self.timing_phase = 0.0;
        self.timing_rate_per_sym = 0.0;
        self.last_marker_sym_k = None;
        self.interrupted = false;
        self.last_good_cursor = 0;
        self.realign_on_next_marker = false;
        self.bulk_drift_seeded = false;
        self.bulk_drift_marker_log.clear();
        self.recent_data_syms.clear();
        self.dd_pll.reset();
    }

    /// Refine `timing_phase` by parabolic-fitting the correlation of the
    /// 32-symbol known sync pattern against the underlying fse_input at
    /// three fractional offsets (−1, 0, +1 fse samples) around the
    /// marker's integer position. Only invoked when Golay+CRC has already
    /// validated the marker, so we know the sync pattern is genuinely
    /// present.
    fn refine_timing_from_marker(&mut self, marker_pos: usize) {
        let sync = marker::make_sync_pattern();
        let data_region_start = N_PREAMBLE + HEADER_SYM_COUNT;
        let abs_sym_pos = data_region_start + marker_pos;
        let center_pos = self.fse_start as f64
            + (abs_sym_pos * self.pitch_fse) as f64
            + self.timing_phase;
        // Correlation sample spacing for the parabolic fit: 1 fse sample
        // regardless of profile. Narrow enough that c_±h stays inside the
        // locally-parabolic cap of the RC correlation surface (widening to
        // half-symbol lands on the shoulder where the shape bends and the
        // fit goes noisy). If drift outpaces what ±1 fse can resolve
        // directly, the fit saturates at ±1; the per-symbol ramp then
        // picks up the residual over the next few markers.
        let h: f64 = 1.0;
        let corr_mag = |fse_input: &[Complex64], delta: f64| -> Option<f64> {
            let mut sum = Complex64::new(0.0, 0.0);
            for k in 0..MARKER_SYNC_LEN {
                let pos = center_pos + (k * self.pitch_fse) as f64 + delta;
                let y = interp_fse(fse_input, pos)?;
                sum += y * sync[k].conj();
            }
            Some(sum.norm())
        };
        let (c_minus, c_zero, c_plus) = match (
            corr_mag(&self.fse_input, -h),
            corr_mag(&self.fse_input, 0.0),
            corr_mag(&self.fse_input, h),
        ) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return,
        };
        // Parabolic peak on samples spaced by `h` fse:
        //     δ* = h · (c_− − c_+) / [2·(c_− − 2·c_0 + c_+)]
        let denom = c_minus - 2.0 * c_zero + c_plus;
        if denom.abs() < 1e-9 {
            return;
        }
        let delta_opt = h * (c_minus - c_plus) / (2.0 * denom);
        // Clamp expressed in SYMBOL units so the threshold scales with
        // pitch_fse — a fraction of a symbol is the meaningful quantity
        // regardless of how many fse samples per symbol we happen to have.
        let max_step_fse = MAX_TIMING_STEP_FRAC * self.pitch_fse as f64;
        if !delta_opt.is_finite() || delta_opt.abs() > max_step_fse {
            return;
        }
        let current_sym = data_region_start + marker_pos;
        if self.realign_on_next_marker {
            // First marker after a silence/squelch interruption: this is
            // the only data-aided ground truth we have for where the TX
            // thinks we are. Take the measurement at face value (α=1.0)
            // and restart the rate estimator on a fresh interval — the
            // pre-gap rate is stale, and we don't know what d_sym is
            // relative to what the TX sent during the gap anyway.
            self.timing_phase += delta_opt;
            self.realign_on_next_marker = false;
            self.last_marker_sym_k = Some(current_sym);
            // Keep `timing_rate_per_sym` unchanged — the TX↔RX clock
            // mismatch itself hasn't changed across a squelch gap; it's
            // only the cumulative phase that does, and we just fixed that.
            return;
        }
        // Rate update: only when the bulk-drift LS regression hasn't yet
        // produced a rate estimate. Once seeded, the regression's slope
        // is the precision source (±1–2 ppm from O(20+) markers); a
        // single-interval LPF of `delta_opt / d_sym` injects pure
        // parabolic-fit noise that flattens ~half of the regression's
        // precision per marker (β=0.5). Phase residual still corrected
        // via the α-term below either way.
        if !self.bulk_drift_seeded {
            if let Some(prev_sym) = self.last_marker_sym_k {
                let d_sym = current_sym.saturating_sub(prev_sym);
                if d_sym > 0 {
                    let rate_correction = delta_opt / d_sym as f64;
                    self.timing_rate_per_sym +=
                        TIMING_UPDATE_BETA * (rate_correction - self.timing_rate_per_sym);
                    let max_rate_fse_per_sym =
                        MAX_TIMING_RATE_PER_SYM_SYM * self.pitch_fse as f64;
                    self.timing_rate_per_sym = self
                        .timing_rate_per_sym
                        .clamp(-max_rate_fse_per_sym, max_rate_fse_per_sym);
                }
            }
        }
        self.timing_phase += TIMING_UPDATE_ALPHA * delta_opt;
        self.last_marker_sym_k = Some(current_sym);
    }

    fn walk_markers(&mut self) {
        if self.walker_stopped {
            return;
        }
        let data_region_start = N_PREAMBLE + HEADER_SYM_COUNT;
        if self.syms.len() <= data_region_start {
            return;
        }
        // Post-silence: engage the wide search window immediately so the
        // first scan has a real chance of catching the marker even if the
        // gap nudged it a few dozen symbols past where we expected.
        if self.realign_on_next_marker {
            self.consecutive_fails = self.consecutive_fails.max(2);
        }

        loop {
            let data_region_len = self.syms.len() - data_region_start;
            if self.cursor + MARKER_LEN > data_region_len {
                break;
            }

            let search_window = if self.consecutive_fails >= 2 {
                WIDE_WINDOW
            } else {
                NARROW_WINDOW
            };
            // We need cursor + search_window + MARKER_SYNC_LEN available to
            // scan the full window. Shorter would mean guessing on a partial
            // window, which during streaming would race the next feed — just
            // wait instead.
            let scan_end = self.cursor + search_window + MARKER_SYNC_LEN;
            if scan_end > data_region_len {
                break;
            }

            let data_region = &self.syms[data_region_start..];
            let hit = marker::find_sync_in_window(
                data_region,
                self.cursor,
                search_window,
                0.5,
            );
            // On fail OR bogus payload we do the same accounting: bump the
            // fail counter, advance cursor, and flip into `interrupted` mode
            // (which freezes the adaptive loops in extend_syms) once we're
            // confident the stream has dropped. On interruption entry we
            // rewind the cursor back to the last successfully-decoded
            // marker position so a WIDE_WINDOW scan from there catches the
            // first real post-interruption marker — instead of walking
            // blindly through silence/interference and stepping past it.
            let (marker_pos, _gain) = match hit {
                Some(h) => h,
                None => {
                    self.consecutive_fails += 1;
                    self.segments_lost += 1;
                    self.cursor += MARKER_LEN;
                    if self.consecutive_fails == INTERRUPTION_FAILS {
                        // First fall over the interruption threshold.
                        // Rewind to the last successful marker and flip
                        // into frozen-adaptive mode so future feeds keep
                        // sweeping cleanly from that anchor.
                        self.interrupted = true;
                        self.cursor = self.last_good_cursor;
                    }
                    continue;
                }
            };
            if marker_pos + MARKER_LEN > data_region.len() {
                break;
            }
            let marker_syms = &data_region[marker_pos..marker_pos + MARKER_LEN];
            let payload = match marker::decode_marker_at(marker_syms) {
                Some(p) => p,
                None => {
                    self.consecutive_fails += 1;
                    self.segments_lost += 1;
                    self.cursor = marker_pos + MARKER_LEN;
                    if self.consecutive_fails == INTERRUPTION_FAILS {
                        self.interrupted = true;
                        self.cursor = self.last_good_cursor;
                    }
                    continue;
                }
            };
            // Fresh marker validated (Golay+CRC8 passed). If we were in an
            // interrupted regime, this is the first trustworthy anchor on
            // the other side of the gap — unfreeze the adaptive loops and
            // flag the timing update path to hard-realign against this
            // marker's known sync pattern instead of a smoothed α-step.
            if self.interrupted {
                self.interrupted = false;
                self.realign_on_next_marker = true;
            }
            self.consecutive_fails = 0;

            match self.session_id_low_lock {
                None => self.session_id_low_lock = Some(payload.session_id_low),
                Some(locked) if locked != payload.session_id_low => {
                    // Different session in the same stream — same policy as
                    // rx_v2_single: stop here (multi-session merging is a
                    // higher-layer concern).
                    self.walker_stopped = true;
                    break;
                }
                _ => {}
            }

            // Determine segment length.
            let n_cw = if payload.is_meta() {
                1
            } else if let Some(ref ah) = self.app_hdr {
                let total_data_cw =
                    (((ah.file_size as usize) + self.k_bytes - 1) / self.k_bytes) as u32;
                let remaining = total_data_cw.saturating_sub(payload.base_esi);
                V2_CODEWORDS_PER_SEGMENT
                    .min(remaining as usize)
                    .max(1)
            } else {
                V2_CODEWORDS_PER_SEGMENT
            };
            let data_sym_count = n_cw * self.syms_per_cw;
            let n_pilot_groups = (data_sym_count + D_SYMS - 1) / D_SYMS;
            let seg_sym_len = data_sym_count + n_pilot_groups * P_SYMS;
            let seg_end = marker_pos + MARKER_LEN + seg_sym_len;

            if seg_end > data_region_len {
                // Not enough data yet for the segment. Do NOT advance cursor:
                // next feed will re-find the exact same marker at the same
                // position (marker detection is deterministic) and retry.
                break;
            }

            let seg_syms_raw = &data_region[marker_pos + MARKER_LEN..seg_end];
            let mut seg_data_syms =
                track_segment(seg_syms_raw, &mut self.sigma2_sum, &mut self.sigma2_count);

            if seg_data_syms.len() < n_cw * self.syms_per_cw {
                self.segments_lost += 1;
                self.cursor = seg_end;
                continue;
            }

            // Per-symbol DD-PLL on top of the pilot-aided gain correction.
            // Pilots handle slow phase/magnitude drift at 34-sym cadence;
            // the PLL handles residual fast phase noise that would
            // otherwise smear 16-APSK inner/outer ring boundaries.
            // Decision from the data constellation (meta & data share
            // constellation — QPSK for ULTRA/ROBUST, 8-PSK for NORMAL,
            // 16-APSK for HIGH/MEGA).
            for sym in seg_data_syms.iter_mut() {
                let idx = self.constellation.slice_nearest(&[*sym])[0];
                let decision = self.constellation.points[idx];
                *sym = self.dd_pll.derotate_and_update(*sym, decision);
            }

            // Harvest a subsampled slice of this segment's corrected data
            // symbols for the GUI constellation view. Non-meta segments
            // only — meta is always QPSK-ish (1 CW, single profile) and
            // would visually contaminate the main-modulation cluster.
            if !payload.is_meta() {
                for k in (0..seg_data_syms.len()).step_by(RECENT_DATA_SYMS_STRIDE) {
                    if self.recent_data_syms.len() >= RECENT_DATA_SYMS_CAP {
                        self.recent_data_syms.pop_front();
                    }
                    self.recent_data_syms.push_back(seg_data_syms[k]);
                }
            }

            let sigma2_for_llr = if self.sigma2_count > 0 {
                (self.sigma2_sum / self.sigma2_count as f64).max(1e-6)
            } else {
                0.1
            };

            for cw_idx in 0..n_cw {
                let off = cw_idx * self.syms_per_cw;
                let cw_syms = &seg_data_syms[off..off + self.syms_per_cw];
                let llr = soft_demod::llr_maxlog(cw_syms, &self.constellation, sigma2_for_llr);
                let llr_deint = interleaver::apply_permutation_f32(&llr, &self.deinterleave_perm);
                let (info_bytes, converged) = self.decoder.decode_to_bytes(&llr_deint);
                let bytes = info_bytes[..self.k_bytes].to_vec();

                self.total_blocks += 1;
                if converged {
                    self.converged_blocks += 1;
                }

                if payload.is_meta() {
                    if converged {
                        if let Some(h) = app_header::decode_meta_payload(&bytes) {
                            self.app_hdr = Some(h);
                        }
                    }
                } else if converged {
                    let esi = payload.base_esi + cw_idx as u32;
                    self.cw_bytes.insert(esi, bytes);
                }
            }

            // Log this validated marker for the continuous bulk-drift LS
            // regression. Seed refresh runs at every new marker (not just
            // the first BULK_DRIFT_MARKER_COUNT) — noise on the rate
            // estimate scales as ~1/sqrt(N), so the longer the session,
            // the tighter the estimate. Log is capped at a sliding window
            // to bound memory and let the estimate track drift-over-time.
            let is_meta_for_seed = payload.is_meta();
            self.segments_decoded += 1;
            self.new_markers.push(payload);
            self.cursor = seg_end;
            // Anchor for recovery: if the stream is interrupted after this
            // point, the walker rewinds to this cursor so WIDE_WINDOW
            // scans start from solid ground.
            self.last_good_cursor = seg_end;

            if self.enable_bulk_drift_seed {
                let data_region_start = N_PREAMBLE + HEADER_SYM_COUNT;
                let abs_sym_pos = data_region_start + marker_pos;
                let integer_fse_pos = self.fse_start as f64
                    + (abs_sym_pos * self.pitch_fse) as f64
                    + self.timing_phase;
                let sub_sample_offset =
                    self.measure_marker_sub_sample_offset(integer_fse_pos)
                        .unwrap_or(0.0);
                let fse_pos = integer_fse_pos + sub_sample_offset;
                self.bulk_drift_marker_log.push((fse_pos, is_meta_for_seed));
                if self.bulk_drift_marker_log.len() > BULK_DRIFT_LOG_MAX {
                    self.bulk_drift_marker_log.remove(0);
                }
                self.try_seed_bulk_drift();
            }

            // Marker is trusted (Golay+CRC8 + LDPC passed): use its known
            // 32-sym sync pattern as a data-aided timing reference. Updates
            // `timing_phase` by a sub-sample parabolic fit so the FFE
            // sampler follows TX/RX clock drift segment-by-segment. Done
            // last so the `&data_region` borrow earlier in the loop body
            // has been released. Skipped while the bulk-drift seed hasn't
            // produced its first rate estimate yet: during that window the
            // sub-sample offsets logged for regression must reflect raw
            // accumulated drift, not an estimate that Gardner has
            // partially compensated.
            if self.bulk_drift_seeded {
                self.refine_timing_from_marker(marker_pos);
            }
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Linear-interpolate `fse_input` at a fractional position. Returns `None`
/// if the position falls outside the available buffer (caller should skip
/// the Gardner TED on that symbol).
fn interp_fse(fse_input: &[Complex64], pos: f64) -> Option<Complex64> {
    if pos < 0.0 {
        return None;
    }
    let lo = pos.floor() as usize;
    let hi = lo + 1;
    if hi >= fse_input.len() {
        return None;
    }
    let frac = pos - lo as f64;
    Some(fse_input[lo] * (1.0 - frac) + fse_input[hi] * frac)
}

/// Pilot-aided complex-gain (magnitude + phase) interpolation on one segment.
///
/// Replicates the body of `rx_v2::track_segment` (which is module-private).
/// Maintains an accumulator for the per-pilot sigma² estimate used by the
/// LLR computation downstream.
fn track_segment(
    seg_syms: &[Complex64],
    sigma2_sum: &mut f64,
    sigma2_count: &mut usize,
) -> Vec<Complex64> {
    let group_sz = D_SYMS + P_SYMS;
    let n_groups = seg_syms.len() / group_sz;

    let mut pilot_gains: Vec<(usize, Complex64)> = Vec::with_capacity(n_groups);
    for g in 0..n_groups {
        let offset = g * group_sz;
        let pilot_start = offset + D_SYMS;
        let pilot_end = pilot_start + P_SYMS;
        let pilots_tx = pilot::pilots_for_group(g);
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..P_SYMS {
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
        return seg_syms
            .iter()
            .enumerate()
            .filter(|(i, _)| i % group_sz < D_SYMS)
            .map(|(_, &s)| s)
            .collect();
    }

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
        let is_pilot = inner >= D_SYMS;
        let (mag, phase) = interp(i);
        let inv_gain = Complex64::from_polar(1.0 / mag.max(1e-6), -phase);
        let y_corrected = y_raw * inv_gain;

        if is_pilot {
            let group = i / group_sz;
            let pilots_tx = pilot::pilots_for_group(group);
            let expected = pilots_tx[inner - D_SYMS];
            *sigma2_sum += (y_corrected - expected).norm_sqr();
            *sigma2_count += 1;
        } else {
            data_syms.push(y_corrected);
        }
    }

    data_syms
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_header::mime;
    use crate::modulator;
    use crate::payload_envelope::PayloadEnvelope;

    fn fnv1a_u16(data: &[u8]) -> u16 {
        let mut h: u32 = 2166136261;
        for &b in data {
            h ^= b as u32;
            h = h.wrapping_mul(16777619);
        }
        (h ^ (h >> 16)) as u16
    }

    fn make_tx(data: &[u8], profile: ProfileIndex, session_id: u32) -> Vec<f32> {
        let config = profile.to_config();
        let envelope = PayloadEnvelope::new("test.bin", "HB9TST", data.to_vec()).unwrap();
        let wire = envelope.encode();
        let hash = fnv1a_u16(&wire);
        let symbols = frame::build_superframe_v2(&wire, &config, session_id, mime::BINARY, hash);
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau).unwrap();
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, config.center_freq_hz)
    }

    /// Loopback NORMAL: feed the entire TX in one call and verify the decoder
    /// reaches full recovery (header, AppHeader, assembled payload).
    #[test]
    fn loopback_normal_one_feed() {
        let data: Vec<u8> = (0..600).map(|i| (i * 7 + 3) as u8).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0xFEED_CAFE);
        let mut dec = StreamingDecoder::new(ProfileIndex::Normal);
        dec.feed_samples(&tx);
        assert!(dec.is_initialized(), "init should succeed on NORMAL loopback");
        assert!(dec.header().is_some(), "header must decode");
        assert!(dec.app_header().is_some(), "AppHeader must decode");
        assert!(dec.all_data_present(), "all data blocks should recover");
        let assembled = dec.try_assemble().expect("assemble ok");
        let env = PayloadEnvelope::decode_or_fallback(&assembled);
        assert_eq!(env.content, data);
    }

    /// Same payload but fed in 10 ms chunks: the incremental pipeline must
    /// reach the same final state as the one-feed path.
    #[test]
    fn loopback_normal_incremental_chunks() {
        let data: Vec<u8> = (0..600).map(|i| (i * 11 + 5) as u8).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0xDEAD_BEEF);
        let mut dec = StreamingDecoder::new(ProfileIndex::Normal);
        let chunk = (AUDIO_RATE / 100) as usize;
        for c in tx.chunks(chunk) {
            dec.feed_samples(c);
        }
        assert!(dec.is_initialized());
        assert!(dec.all_data_present());
        let assembled = dec.try_assemble().expect("assemble ok");
        let env = PayloadEnvelope::decode_or_fallback(&assembled);
        assert_eq!(env.content, data);
    }

    /// Apply a synthetic sound-card ppm drift to an already-modulated TX
    /// waveform. Linear interpolation resampler, same shape as the one the
    /// streaming decoder uses internally — so we exercise the compensation
    /// loop against the same kind of distortion a real USB audio card
    /// produces.
    fn apply_drift_ppm(samples: &[f32], drift_ppm: f64) -> Vec<f32> {
        let ratio = 1.0 + drift_ppm * 1e-6;
        let n_out = ((samples.len() as f64) / ratio) as usize;
        let mut out = Vec::with_capacity(n_out);
        for i in 0..n_out {
            let t = i as f64 * ratio;
            let idx = t.floor() as usize;
            let frac = (t - idx as f64) as f32;
            if idx + 1 < samples.len() {
                out.push(samples[idx] * (1.0 - frac) + samples[idx + 1] * frac);
            } else if idx < samples.len() {
                out.push(samples[idx]);
            } else {
                break;
            }
        }
        out
    }

    /// Long NORMAL TX with 50 ppm of synthetic clock drift injected. The
    /// Gardner PI loop should track the drift continuously and keep the
    /// symbol grid aligned so ≥ 95 % of LDPC blocks decode.
    ///
    /// Sign convention: `apply_drift_ppm(+50)` time-COMPRESSES the waveform
    /// (output is shorter), so RX-frame symbol centres land EARLIER than the
    /// 32-sample grid expects. The streaming decoder should therefore pull
    /// its sampling EARLIER — `timing_phase` goes negative.
    #[test]
    fn streaming_compensates_50ppm_drift() {
        let data: Vec<u8> = (0..3000).map(|i| ((i * 31 + 7) as u8)).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0x5A5A_A5A5);
        let drifted = apply_drift_ppm(&tx, 50.0);

        let mut dec = StreamingDecoder::new(ProfileIndex::Normal);
        let chunk = (AUDIO_RATE / 2) as usize;
        for c in drifted.chunks(chunk) {
            dec.feed_samples(c);
        }
        assert!(dec.is_initialized(), "init must succeed under 50 ppm drift");
        let converged = dec.data_blocks_recovered();
        let expected = dec.expected_data_blocks();
        assert!(expected > 0, "AppHeader must be recovered");
        assert!(
            converged * 20 >= expected * 19,
            "50 ppm drift: only {converged}/{expected} data blocks recovered \
             (timing_phase={:.4})",
            dec.timing_phase()
        );
        assert!(
            dec.timing_phase() < -0.05,
            "timing_phase {:.4} should have gone negative under +50 ppm \
             drift (RX sees time-compressed audio)",
            dec.timing_phase()
        );
    }

    /// 100 ppm of drift is near the worst case a cheap USB audio dongle
    /// produces. Streaming must still recover ≥ 90 % of data blocks.
    #[test]
    fn streaming_compensates_100ppm_drift() {
        let data: Vec<u8> = (0..2500).map(|i| ((i * 37 + 11) as u8)).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0xFACE_D00D);
        let drifted = apply_drift_ppm(&tx, 100.0);

        let mut dec = StreamingDecoder::new(ProfileIndex::Normal);
        let chunk = (AUDIO_RATE / 2) as usize;
        for c in drifted.chunks(chunk) {
            dec.feed_samples(c);
        }
        let converged = dec.data_blocks_recovered();
        let expected = dec.expected_data_blocks();
        assert!(expected > 0, "AppHeader must be recovered");
        assert!(
            converged * 10 >= expected * 9,
            "100 ppm drift: only {converged}/{expected} data blocks recovered \
             (timing_phase={:.4})",
            dec.timing_phase()
        );
    }

    /// MEGA uses `pitch_fse=15` (d_fse=2, heavily oversampled), so the
    /// per-symbol drift measured in fse_input samples is ~7.5× the NORMAL
    /// case at the same ppm. The correlation window and rate clamp have to
    /// scale with pitch_fse for this to work.
    #[test]
    fn streaming_compensates_50ppm_drift_mega() {
        let data: Vec<u8> = (0..3000).map(|i| ((i * 43 + 17) as u8)).collect();
        let tx = make_tx(&data, ProfileIndex::Mega, 0x8888_0011);
        let drifted = apply_drift_ppm(&tx, 50.0);

        let mut dec = StreamingDecoder::new(ProfileIndex::Mega);
        let chunk = (AUDIO_RATE / 2) as usize;
        for c in drifted.chunks(chunk) {
            dec.feed_samples(c);
        }
        let converged = dec.data_blocks_recovered();
        let expected = dec.expected_data_blocks();
        assert!(expected > 0, "AppHeader must be recovered");
        assert!(
            converged * 10 >= expected * 9,
            "MEGA 50 ppm drift: only {converged}/{expected} data blocks \
             recovered (timing_phase={:.4})",
            dec.timing_phase()
        );
    }

    /// Negative drift: `apply_drift_ppm(-50)` time-EXPANDS the waveform, so
    /// symbol centres land LATER than the 32-sample grid expects. The loop
    /// should pull sampling LATER — `timing_phase` goes positive.
    #[test]
    fn streaming_compensates_negative_drift() {
        let data: Vec<u8> = (0..2500).map(|i| ((i * 41 + 13) as u8)).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0xA0A0_5050);
        let drifted = apply_drift_ppm(&tx, -50.0);

        let mut dec = StreamingDecoder::new(ProfileIndex::Normal);
        let chunk = (AUDIO_RATE / 2) as usize;
        for c in drifted.chunks(chunk) {
            dec.feed_samples(c);
        }
        let converged = dec.data_blocks_recovered();
        let expected = dec.expected_data_blocks();
        assert!(expected > 0, "AppHeader must be recovered");
        assert!(
            converged * 20 >= expected * 19,
            "-50 ppm drift: only {converged}/{expected} data blocks recovered \
             (timing_phase={:.4})",
            dec.timing_phase()
        );
        assert!(
            dec.timing_phase() > 0.05,
            "timing_phase {:.4} should have gone positive under -50 ppm drift \
             (RX sees time-expanded audio)",
            dec.timing_phase()
        );
    }

    /// Baseline: streaming recovers all blocks on a clean 5 kB TX.
    /// Establishes what to expect when the squelch-gap test runs with
    /// the same waveform minus 300 ms of audio in the middle.
    #[test]
    fn streaming_5kb_no_gap_baseline() {
        let data: Vec<u8> = (0..5000).map(|i| (i * 13) as u8).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0x5EC5_E501);
        let mut dec = StreamingDecoder::new(ProfileIndex::Normal);
        let chunk = (AUDIO_RATE / 2) as usize;
        for c in tx.chunks(chunk) {
            dec.feed_samples(c);
        }
        let streamed = dec.data_blocks_recovered();
        let expected = dec.expected_data_blocks();
        eprintln!("5kB no gap: {streamed}/{expected}");
        assert_eq!(streamed, expected, "baseline must decode all blocks");
    }

    /// Simulate a squelch-gap interruption: a few hundred milliseconds of
    /// zeroed-out audio inserted in the middle of a TX. The decoder should
    /// skip only the segments that overlap the gap and keep the pre- and
    /// post-gap blocks intact — otherwise the adaptive loops (FFE DD-LMS,
    /// timing ramp) have been pulled into garbage across the gap.
    #[test]
    fn streaming_survives_squelch_gap() {
        let data: Vec<u8> = (0..5000).map(|i| (i * 13) as u8).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0x5EC5_E501);

        let mut noisy = tx.clone();
        let gap_start = noisy.len() / 3;
        let gap_len = (0.3 * AUDIO_RATE as f64) as usize;
        for s in noisy.iter_mut().skip(gap_start).take(gap_len) {
            *s = 0.0;
        }

        let mut dec = StreamingDecoder::new(ProfileIndex::Normal);
        let chunk = (AUDIO_RATE / 2) as usize;
        for c in noisy.chunks(chunk) {
            dec.feed_samples(c);
        }
        let streamed = dec.data_blocks_recovered();
        let expected = dec.expected_data_blocks();
        assert!(expected > 0, "AppHeader must be recovered across the gap");
        // Threshold: half the blocks. A 300 ms gap at 1500 Bd at most wipes
        // ~1 data segment (2 blocks) plus adjacent recovery transient,
        // so we should clear 60 %+ comfortably.
        assert!(
            streamed * 2 >= expected,
            "streaming only got {streamed}/{expected} after squelch gap",
        );
    }

    /// The live-worker cadence is ~500 ms batches. Verify the decoder's state
    /// is independent of the batch granularity: feeding the same audio in
    /// 500-ms chunks must agree with a single-batch feed on (data_blocks_recovered,
    /// app_header, assembled hash).
    #[test]
    fn chunked_feeds_match_single_feed() {
        let data: Vec<u8> = (0..400).map(|i| (i * 13) as u8).collect();
        let tx = make_tx(&data, ProfileIndex::Normal, 0xC0DE_0042);

        let mut dec_single = StreamingDecoder::new(ProfileIndex::Normal);
        dec_single.feed_samples(&tx);

        let mut dec_chunked = StreamingDecoder::new(ProfileIndex::Normal);
        let chunk = (AUDIO_RATE / 2) as usize; // 500 ms
        for c in tx.chunks(chunk) {
            dec_chunked.feed_samples(c);
        }

        assert_eq!(dec_single.is_initialized(), dec_chunked.is_initialized());
        assert_eq!(
            dec_single.data_blocks_recovered(),
            dec_chunked.data_blocks_recovered()
        );
        assert_eq!(
            dec_single.app_header().map(|a| a.file_size),
            dec_chunked.app_header().map(|a| a.file_size),
        );
        let a = dec_single.try_assemble().unwrap();
        let b = dec_chunked.try_assemble().unwrap();
        assert_eq!(a, b);
    }
}
