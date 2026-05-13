//! Streaming audio → symbol front-end for the 2x family.
//!
//! Replaces the batch `audio_to_symbols` whole-buffer pipeline with a
//! stateful, chunk-by-chunk variant suitable for live capture. The four
//! pieces wired together are exactly the DVB-S2X reference architecture:
//!
//! ```text
//!   audio chunk (f32, mono, 48 kHz)
//!      │
//!      ▼
//!   NCO downmix  (continuous phase across chunks via absolute sample
//!                 counter — no per-chunk phase reset)
//!      │
//!      ▼
//!   RRC matched filter  (re-applied on a small overlap-save buffer each
//!                        chunk; FIR is mode='same', the front-edge warm-
//!                        up is covered by the previous chunk's tail)
//!      │
//!      ▼
//!   Farrow cubic-Lagrange interpolation at fractional strobe positions
//!      │
//!      ▼
//!   Gardner / AbsGardner TED + PI loop (per emitted strobe → fractional
//!                                       correction added to strobe step)
//!      │
//!      ▼
//!   timing-recovered Vec<Complex64> symbols
//! ```
//!
//! What this replaces: the batch `audio_to_symbols` SOF-anchored phase
//! pick + naive integer-step sampling. The streaming variant tracks
//! drift continuously, so a >50 ppm clock mismatch on sound-card paths
//! no longer accumulates over a single PLHEADER cycle and lossy decodes
//! one CW out of every four.
//!
//! What this does NOT do: the symbol-domain state machine. Once symbols
//! land in the caller's buffer, the caller still drives `rx_v4_symbols`
//! periodically (idempotent on a growing buffer). Closed-loop timing
//! makes the periodic re-decode lossless — every CW is sampled once,
//! correctly, regardless of when `rx_v4_symbols` is called.
//!
//! Memory: the internal audio buffer is bounded at
//! `taps.len() + sps + small margin` (~400 samples for HIGH2X), trimmed
//! after every chunk. No unbounded growth.

use std::f64::consts::PI;

use modem_core_base::demodulator;
use modem_core_base::farrow;
use modem_core_base::rrc::{self, rrc_taps};
use modem_core_base::timing_loop::{TedVariant, TimingLoop};
use modem_core_base::types::{Complex64, AUDIO_RATE, RRC_SPAN_SYM};
use modem_core2x::frame2x::{cycle_pilot_map, full_cycle_len_syms};
use modem_core2x::profile2x::ModemConfig2x;

/// Stateful audio → symbol front-end. Construct once per RX session,
/// feed audio chunks via [`process_chunk`].
pub struct StreamingFrontend {
    /// RRC matched-filter taps (same as TX).
    taps: Vec<f64>,
    /// Half-tap-length, for FIR-warmup accounting.
    taps_half: usize,
    /// Nominal samples per symbol (integer from `rrc::check_integer_constraints`).
    sps: usize,
    /// Carrier centre frequency (Hz). Hot path constant.
    center_freq_hz: f64,
    /// Absolute index of the next NCO sample to consume. Lets the NCO
    /// phase stay continuous when the working audio buffer is trimmed
    /// between chunks.
    nco_base_sample: u64,
    /// Working audio buffer. Holds the trimmed tail of the previous
    /// chunk (FIR warmup + strobe context) plus any new chunk samples
    /// not yet strobed.
    audio_buffer: Vec<f32>,
    /// Fractional position of the NEXT strobe inside `audio_buffer`. In
    /// the same sample-index space the MF output uses (mode='same'
    /// means MF index ↔ audio index 1:1).
    strobe_pos: f64,
    /// Closed-loop timing recovery. The TED variant is chosen per-strobe
    /// in `process_chunk` (Gardner on data, AbsGardner on pilot symbols
    /// once we have a SOF anchor); this field stores the default Gardner
    /// variant for the pre-anchor acquisition phase and the data path.
    timing_loop: TimingLoop,

    /// Absolute index of the next symbol the frontend will emit (counted
    /// from session start). Drives [`StreamingFrontend::is_pilot_symbol`]
    /// once `sof_anchor` is set.
    symbols_emitted: u64,
    /// Absolute symbol index where the SOF was located. When set, the
    /// per-strobe TED switches from plain Gardner to AbsGardner on the
    /// 34 interior symbols of every 36-sym pilot block. Set explicitly
    /// via [`StreamingFrontend::align_to_sof`]; remains None otherwise
    /// (no auto-SOF probe inside this struct — the worker decides when
    /// to call it, e.g. after a successful `rx_v4_symbols`).
    sof_anchor: Option<u64>,
    /// Pre-computed per-cycle pilot map (length = cycle_len_syms). Same
    /// for every cycle in the burst.
    pilot_lookup: Vec<bool>,
    /// Length of one full PLHEADER cycle in symbols. Snapshotted at
    /// construction so the hot path doesn't touch ModemConfig2x.
    cycle_len_syms: u64,
}

impl StreamingFrontend {
    /// Construct a front-end for `cfg`. Aligns the initial strobe
    /// position on `taps.len() / 2`, which is where the TX modulator
    /// places the pulse peak for symbol 0 (mirror of the batch
    /// `audio_to_symbols` integer-step lead-in).
    pub fn new(cfg: ModemConfig2x) -> Self {
        let (sps, _pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau)
                .expect("profile must have integer-constrained (Rs, τ)");
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        let taps_half = taps.len() / 2;
        // Default TED is the classic Gardner across every 2x
        // constellation. Plain Gardner alone tracks 50–100 ppm on all
        // 2x profiles without the data-induced drift that AbsGardner
        // exhibits on multi-ring APSK (DVB-S2X §9.3.2 gives the well-
        // known recipe of using AbsGardner only on unit-magnitude pilot
        // symbols). Once we have a SOF anchor (a few PLHEADER cycles
        // in), the per-strobe TED selection in `process_chunk` switches
        // to AbsGardner on the 36-sym `(1+j)/√2` blocks where it is
        // unbiased — the D-1c-ii fix that extends robust tracking past
        // 100 ppm on APSK profiles.
        let ted = TedVariant::Gardner;
        let pilot_lookup = cycle_pilot_map(&cfg);
        let cycle_len_syms = full_cycle_len_syms(&cfg) as u64;
        Self {
            center_freq_hz: cfg.base.center_freq_hz,
            taps,
            taps_half,
            sps,
            nco_base_sample: 0,
            audio_buffer: Vec::new(),
            // Match the batch `audio_to_symbols` indexing: strobe k
            // lands on MF[k·sps]. Symbol 0 of the burst lives at index
            // 0 (with a few samples of FIR warm-up noise before
            // anything real shows up; the downstream `rx_v4_symbols`
            // SOF search walks past that). The first few strobes fall
            // outside Farrow's 4-tap window — `interp_safe` falls back
            // to nearest-sample lookup there, which is exactly what
            // the batch path was doing implicitly.
            strobe_pos: 0.0,
            timing_loop: TimingLoop::with_defaults(ted),
            symbols_emitted: 0,
            sof_anchor: None,
            pilot_lookup,
            cycle_len_syms,
        }
    }

    /// Nominal samples per symbol for this profile. Useful for tests.
    #[inline]
    pub fn sps(&self) -> usize {
        self.sps
    }

    /// Read-only access to the underlying TimingLoop for diagnostics
    /// (`last_err`, `integ` — the steady-state drift estimate).
    #[inline]
    pub fn timing_loop(&self) -> &TimingLoop {
        &self.timing_loop
    }

    /// Absolute symbol index of the most recently detected SOF, if any.
    /// Visible for tests / diagnostics; once set, the front-end gates
    /// AbsGardner on pilot symbols.
    #[inline]
    pub fn sof_anchor(&self) -> Option<u64> {
        self.sof_anchor
    }

    /// Total symbols emitted since session start. Useful for tests that
    /// want to derive absolute pilot positions.
    #[inline]
    pub fn symbols_emitted(&self) -> u64 {
        self.symbols_emitted
    }

    /// Enable pilot-aided TED by pinning the SOF anchor at absolute
    /// symbol index `absolute_symbol_idx` (in the same space as
    /// [`StreamingFrontend::symbols_emitted`]). Once set, the loop runs
    /// AbsGardner on every 34-symbol-wide pilot interior (the only
    /// strobes where EARLY and LATE Farrow lookups span pure pilot
    /// territory, so the TED is data-independent and unbiased — the
    /// DVB-S2X §9.3.2 reference). Strobes outside pilot interiors keep
    /// using plain Gardner. Without this call the front-end stays in
    /// Gardner-only mode (the original Phase D-1c behaviour, unchanged).
    ///
    /// The expected caller is the worker after `rx_v4_symbols` returned
    /// a `RxResult2x`: convert the result's SOF position to the absolute
    /// space and forward it here. Re-calling is fine — anchors that match
    /// the same cycle layout are equivalent (the lookup uses `%`
    /// `cycle_len_syms`); a fresh anchor on a different cycle just shifts
    /// the pilot indices forward by an integer number of cycles.
    #[inline]
    pub fn align_to_sof(&mut self, absolute_symbol_idx: u64) {
        self.sof_anchor = Some(absolute_symbol_idx);
    }

    /// Drop the current SOF anchor. The front-end falls back to plain
    /// Gardner (no pilot interior gating) until a fresh
    /// [`align_to_sof`](Self::align_to_sof) is called.
    ///
    /// Used by the worker between bursts: once the EOT cycle has been
    /// decoded and the session finalised, the SOF anchor of that burst
    /// is no longer meaningful — the next burst starts at a different
    /// (and yet-unknown) absolute symbol position. Clearing here forces
    /// the next `rx_v4_symbols` tick to re-acquire on the new burst's
    /// PLHEADER. The Farrow + Gardner timing-loop state is preserved
    /// (clock drift is continuous between bursts on the same channel).
    #[inline]
    pub fn clear_sof_anchor(&mut self) {
        self.sof_anchor = None;
    }

    /// True when the symbol at absolute index `abs_idx` is a pilot
    /// symbol (one of the 36-sym `(1+j)/√2` blocks) according to the
    /// pre-computed cycle layout, relative to the current SOF anchor.
    /// Returns `false` before SOF lock, or when no anchor has been set.
    #[inline]
    fn is_pilot_symbol(&self, abs_idx: u64) -> bool {
        let Some(anchor) = self.sof_anchor else {
            return false;
        };
        // Before the anchor itself — happens only if we override anchor
        // backwards, which we don't do, but defensively guard anyway.
        if abs_idx < anchor {
            return false;
        }
        let rel = abs_idx - anchor;
        let pos_in_cycle = (rel % self.cycle_len_syms) as usize;
        // pilot_lookup.len() == cycle_len_syms by construction.
        self.pilot_lookup[pos_in_cycle]
    }

    /// True when `abs_idx` is a pilot symbol AND both immediate
    /// neighbours `abs_idx ± 1` are also pilots. Necessary for an
    /// unbiased AbsGardner step: the LATE/EARLY Farrow lookups land at
    /// `strobe ± sps/2` which corresponds to symbol-position `abs_idx ±
    /// 0.5` — i.e. the matched-filter output there is a 50/50 blend of
    /// symbols `abs_idx` and `abs_idx ± 1`. If either neighbour is a
    /// data symbol the EARLY or LATE sample carries data magnitude
    /// (variable on multi-ring APSK) and the TED becomes data-dependent.
    /// Excluding the first and last symbol of each 36-sym pilot block
    /// gives 34 clean strobes per block — still plenty of updates for
    /// the slow DVB-S2 loop gains.
    #[inline]
    fn is_strict_pilot_interior(&self, abs_idx: u64) -> bool {
        if abs_idx == 0 {
            return false;
        }
        self.is_pilot_symbol(abs_idx)
            && self.is_pilot_symbol(abs_idx - 1)
            && self.is_pilot_symbol(abs_idx + 1)
    }


    /// Append `chunk` to the working buffer and emit every strobe whose
    /// Farrow context fits inside the (possibly extended) buffer. Each
    /// emitted strobe walks the Gardner TED forward by one symbol; the
    /// loop integ converges to the per-symbol drift.
    ///
    /// The function returns a `Vec<Complex64>` of newly-produced
    /// symbols. Empty when the chunk wasn't long enough to cross
    /// another strobe boundary (rare, given chunks are ≥ ~500 samples
    /// and `sps ≤ 96`).
    pub fn process_chunk(&mut self, chunk: &[f32]) -> Vec<Complex64> {
        if chunk.is_empty() {
            return Vec::new();
        }
        self.audio_buffer.extend_from_slice(chunk);
        // Memory is bounded by the post-strobe trim below — the buffer
        // never holds more than `chunk.len() + taps_half + sps + 4`
        // samples across two calls. No defensive cap is needed and a
        // cap here would discard un-strobed audio.

        // Downmix the WHOLE working buffer to baseband. NCO phase is
        // anchored on `nco_base_sample` so chunks chain without a phase
        // glitch (a per-chunk reset to zero would create a step
        // proportional to `fc · (trim distance)`).
        let bb = downmix_with_offset(
            &self.audio_buffer,
            self.center_freq_hz,
            self.nco_base_sample,
        );
        // RRC matched filter, mode='same'. For our ≤ ~600-sample
        // working buffer this is the direct O(N·M) path (well below
        // demodulator's FFT_THRESHOLD = 2048), so it stays cheap.
        let mf = demodulator::matched_filter(&bb, &self.taps);

        let half_sps = self.sps as f64 * 0.5;
        let mut symbols = Vec::new();
        loop {
            // We need MF samples up to `strobe_pos + sps/2 + 2` for the
            // LATE Farrow window. If the buffer doesn't extend that far
            // yet, stop and wait for the next chunk.
            let late = self.strobe_pos + half_sps;
            if late + 2.0 >= mf.len() as f64 {
                break;
            }
            let early = self.strobe_pos - half_sps;
            // Gardner runs only when ALL three Farrow lookups land
            // inside the cubic-Lagrange's safe zone (`1 ≤ pos ≤ N-3`).
            // The very first strobes of a fresh frontend (typically just
            // strobe 0, where EARLY = -sps/2 is below the buffer floor)
            // fail this check; they fall through to nearest-sample
            // integer sampling instead — exactly what the batch path
            // does at `mf[0]`. With this gate, the loop integ stays
            // exactly zero across the lead-in symbols, which is the
            // right initial state.
            let farrow_ok = is_farrow_valid(early, mf.len())
                && is_farrow_valid(self.strobe_pos, mf.len())
                && is_farrow_valid(late, mf.len());
            let abs_idx = self.symbols_emitted;
            if farrow_ok {
                let y_early = farrow::interp_complex(&mf, early);
                let y_on = farrow::interp_complex(&mf, self.strobe_pos);
                let y_late = farrow::interp_complex(&mf, late);
                // Pilot-aware TED routing (D-1c-ii). The default TED is
                // classic Gardner — robust on every constellation, slight
                // data-dependent bias on multi-ring APSK but still tracks
                // ≤100 ppm fine, this is the existing Phase D-1c
                // behaviour. When an explicit SOF anchor has been pinned
                // via [`StreamingFrontend::align_to_sof`] AND the current
                // strobe is in the 34-sym INTERIOR of one of the 36-sym
                // `(1+j)/√2` pilot blocks, we swap in AbsGardner instead,
                // which is unbiased on unit-magnitude carriers — the
                // DVB-S2X §9.3.2 recipe. Pilot boundaries (first/last sym
                // of each block) keep Gardner because the EARLY/LATE
                // Farrow lookups there straddle a pilot/data transition
                // and AbsGardner picks up the magnitude jump as a fake
                // timing offset.
                let err = if self.is_strict_pilot_interior(abs_idx) {
                    TedVariant::AbsGardner.error(y_early, y_on, y_late)
                } else {
                    TedVariant::Gardner.error(y_early, y_on, y_late)
                };
                let correction = self.timing_loop.step_with_error(err);
                symbols.push(y_on);
                self.symbols_emitted += 1;
                self.strobe_pos += self.sps as f64 + correction;
            } else {
                // Lead-in / FIR-warmup region: emit the integer-step
                // sample (matches batch `mf[k·sps]`) and advance by
                // exactly sps without touching the TED. The loop integ
                // remains at zero until a real Gardner step runs.
                let i = self.strobe_pos.round() as isize;
                let y = if i >= 0 && (i as usize) < mf.len() {
                    mf[i as usize]
                } else {
                    Complex64::new(0.0, 0.0)
                };
                symbols.push(y);
                self.symbols_emitted += 1;
                self.strobe_pos += self.sps as f64;
            }
        }

        // Trim: keep `taps_half + sps + 4` audio samples BEFORE the
        // next strobe position. The taps_half part keeps the next
        // chunk's MF unbiased at the strobe (mode='same' boundary
        // bias spans `half` samples on each side). The `sps + 4` part
        // covers Farrow's 4-tap window centred on the EARLY sample of
        // the next strobe (`strobe_pos - sps/2 - 1`).
        let keep_back = self.taps_half + self.sps + 4;
        let strobe_floor = (self.strobe_pos as isize) - (keep_back as isize);
        let trim_to = strobe_floor.max(0) as usize;
        if trim_to > 0 {
            self.audio_buffer.drain(..trim_to);
            self.nco_base_sample += trim_to as u64;
            self.strobe_pos -= trim_to as f64;
        }

        symbols
    }
}

/// True when `pos` sits inside Farrow's 4-tap cubic-Lagrange window —
/// i.e. the interpolation needs `samples[n-1, n, n+1, n+2]` with
/// `n = floor(pos)`, so `1 ≤ n` and `n + 2 < len`.
#[inline]
fn is_farrow_valid(pos: f64, len: usize) -> bool {
    let n = pos.floor() as isize;
    n >= 1 && ((n + 2) as usize) < len
}

/// Downmix `samples` to baseband, using `sample_index_offset` as the
/// running NCO phase counter so chunks chain seamlessly.
///
/// `samples[i]` is mixed with carrier `exp(-j·2π·fc·(offset+i)/Fs)`.
/// Identical to [`demodulator::downmix`] when `offset == 0`.
fn downmix_with_offset(
    samples: &[f32],
    center_freq_hz: f64,
    sample_index_offset: u64,
) -> Vec<Complex64> {
    samples
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let n = sample_index_offset.wrapping_add(i as u64);
            let phase = -2.0 * PI * center_freq_hz * n as f64 / AUDIO_RATE as f64;
            Complex64::new(phase.cos(), phase.sin()) * s as f64
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_core2x::frame2x::build_superframe_v4;
    use modem_core2x::profile2x::{
        profile_high_2x, profile_high_plus_2x, profile_high_plus_plus_2x,
        profile_normal_2x, profile_ultra_2x, ProfileIndex2x,
    };
    use modem_core_base::modulator;
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

    fn modulate_for(cfg: &ModemConfig2x, payload: &[u8], session_id: u32) -> Vec<f32> {
        let symbols = build_superframe_v4(payload, cfg, session_id, mime::BINARY, 0xAA55);
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau)
                .unwrap();
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, cfg.base.center_freq_hz)
    }

    #[test]
    fn empty_chunk_returns_no_symbols() {
        let mut sf = StreamingFrontend::new(profile_high_2x());
        let out = sf.process_chunk(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn debug_compare_first_symbols_to_batch() {
        // Sanity check: with the loop *disabled* (kp=ki=0), the streaming
        // strobes must reproduce batch's `mf[k·sps]` exactly.
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0x1);
        let audio = modulate_for(&cfg, &payload, 0xABC);

        // Batch reference: downmix + MF + integer step.
        let bb = demodulator::downmix(&audio, cfg.base.center_freq_hz);
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, 32);
        let mf = demodulator::matched_filter(&bb, &taps);
        let batch_syms: Vec<_> = (0..(mf.len() / 32)).map(|k| mf[k * 32]).collect();

        // Streaming with Gardner DISABLED (we just want to verify the
        // integer-step grid alignment first).
        let mut sf = StreamingFrontend::new(cfg.clone());
        sf.timing_loop = TimingLoop::new(0.0, 0.0, sf.timing_loop.ted());
        let stream_syms = sf.process_chunk(&audio);

        // First 50 symbols should match closely (FIR boundary may differ
        // by 1 symbol due to length rounding).
        let n = stream_syms.len().min(batch_syms.len()).min(50);
        for k in 0..n {
            let d = (stream_syms[k] - batch_syms[k]).norm();
            assert!(
                d < 1e-9,
                "sym {k}: stream={:?} batch={:?} diff={d}",
                stream_syms[k],
                batch_syms[k],
            );
        }
    }

    #[test]
    fn single_chunk_matches_batch_pipeline_high2x() {
        // Encode → modulate → feed audio in a single big chunk through
        // the streaming frontend. The decoder downstream should accept
        // the symbol stream identically to the batch `audio_to_symbols`
        // path (i.e. rx_v4_symbols decodes successfully).
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0xCAFE);
        let audio = modulate_for(&cfg, &payload, 0x1234);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let symbols = sf.process_chunk(&audio);
        assert!(symbols.len() > 100, "expected real symbols, got {}", symbols.len());
        let res = modem_core2x::rx_v4::rx_v4_symbols(&symbols, &cfg)
            .expect("decode through streaming frontend");
        let h = res.app_header.expect("AppHeader recovered");
        assert_eq!(h.session_id, 0x1234);
        assert_eq!(res.data, payload, "payload mismatch after streaming frontend");
    }

    #[test]
    fn chunked_equals_single_chunk_high2x() {
        // Splitting the SAME audio into 500ms chunks and feeding them
        // sequentially MUST produce the same decoded payload as a single
        // big-chunk call. Proves NCO continuity, MF tail handling, and
        // strobe-position book-keeping are correct across boundaries.
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0xBEEF);
        let audio = modulate_for(&cfg, &payload, 0xABCD);

        // Reference: single chunk.
        let mut sf_one = StreamingFrontend::new(cfg.clone());
        let syms_one = sf_one.process_chunk(&audio);
        let res_one = modem_core2x::rx_v4::rx_v4_symbols(&syms_one, &cfg).unwrap();

        // Streaming: split audio in 24 000-sample pieces.
        let mut sf_chunked = StreamingFrontend::new(cfg.clone());
        let mut syms_chunked = Vec::new();
        const CHUNK: usize = 24_000;
        for i in (0..audio.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(audio.len());
            syms_chunked.extend(sf_chunked.process_chunk(&audio[i..end]));
        }

        // Symbol counts may differ by ≤ 1 because of where the strobe
        // falls relative to the buffer trim (the chunked path may not
        // yet have emitted the last symbol when the audio ends). What
        // matters is that the decoded payload is byte-identical.
        let diff = (syms_one.len() as isize - syms_chunked.len() as isize).abs();
        assert!(diff <= 2, "symbol count drift across chunking: {diff}");

        let res_chunked = modem_core2x::rx_v4::rx_v4_symbols(&syms_chunked, &cfg).unwrap();
        let h_one = res_one.app_header.unwrap();
        let h_chunked = res_chunked.app_header.unwrap();
        assert_eq!(h_one.session_id, h_chunked.session_id);
        assert_eq!(res_one.data, res_chunked.data, "chunked decode != single decode");
        assert_eq!(res_chunked.data, payload);
    }

    #[test]
    fn chunked_normal_2x_decodes_correctly() {
        // NORMAL2X has the smallest sps (probably 32 or 48 depending on
        // profile rate) — different from HIGH2X — make sure the
        // streaming path is profile-agnostic.
        let cfg = profile_normal_2x();
        let payload = rng_bytes(400, 0x11);
        let audio = modulate_for(&cfg, &payload, 0x99);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let mut syms = Vec::new();
        const CHUNK: usize = 18_000;
        for i in (0..audio.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(audio.len());
            syms.extend(sf.process_chunk(&audio[i..end]));
        }
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg).expect("decode");
        assert_eq!(res.data, payload);
    }

    #[test]
    fn chunked_ultra_2x_decodes_correctly() {
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(150, 0xDE);
        let audio = modulate_for(&cfg, &payload, 0xCAFE);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let mut syms = Vec::new();
        const CHUNK: usize = 36_000;
        for i in (0..audio.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(audio.len());
            syms.extend(sf.process_chunk(&audio[i..end]));
        }
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg).expect("decode");
        assert_eq!(res.data, payload);
    }

    #[test]
    fn timing_loop_tracks_resampled_audio_50ppm() {
        // Resample the modulated audio to simulate a +50 ppm clock drift
        // between TX and RX, then feed it through the streaming
        // frontend. Without Gardner the integer-step sampler would walk
        // off the strobe; with it the symbol stream still decodes.
        let cfg = profile_high_2x();
        let payload = rng_bytes(300, 0xF0);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let resampled = resample_linear(&audio, 1.0 + 50e-6);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let mut syms = Vec::new();
        const CHUNK: usize = 24_000;
        for i in (0..resampled.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(resampled.len());
            syms.extend(sf.process_chunk(&resampled[i..end]));
        }
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg)
            .expect("decode 50 ppm-drifted audio");
        // TimingLoop integ should have converged near 50 ppm × sps —
        // i.e. roughly `sps · 50e-6` extra samples-per-symbol. Sanity-
        // check it's positive (we resampled UP, so the "true" strobe
        // step is slightly larger than nominal sps).
        let integ = sf.timing_loop().integ();
        assert!(integ > 0.0, "integ should be positive for +ppm, got {integ}");
        assert_eq!(res.data, payload);
    }

    /// Crude linear resampler used only to inject a constant ppm offset
    /// into the test signal. Far less precise than a real timing-
    /// recovered pipeline — the point is to give the StreamingFrontend's
    /// Gardner loop a non-zero error signal to track.
    fn resample_linear(input: &[f32], ratio: f64) -> Vec<f32> {
        let out_len = ((input.len() as f64) * ratio) as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let src = (i as f64) / ratio;
            let i0 = src.floor() as usize;
            let i1 = (i0 + 1).min(input.len() - 1);
            let frac = src - i0 as f64;
            let v = input[i0] as f64 * (1.0 - frac) + input[i1] as f64 * frac;
            out.push(v as f32);
        }
        out
    }

    #[test]
    fn all_profiles_decode_through_streaming_frontend() {
        // Belt-and-braces: every shipped 2x profile must survive the
        // streaming path on a 200-byte payload.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(200, p.as_u8() as u64);
            let audio = modulate_for(&cfg, &payload, 0x42);
            let mut sf = StreamingFrontend::new(cfg.clone());
            let mut syms = Vec::new();
            const CHUNK: usize = 24_000;
            for i in (0..audio.len()).step_by(CHUNK) {
                let end = (i + CHUNK).min(audio.len());
                syms.extend(sf.process_chunk(&audio[i..end]));
            }
            let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg)
                .unwrap_or_else(|| panic!("{p:?} streaming decode returned None"));
            assert_eq!(res.data, payload, "{p:?} streaming roundtrip");
        }
    }

    // --- D-1c-ii pilot-aided TED -----------------------------------------

    #[test]
    fn align_to_sof_default_is_none() {
        // Construction leaves the front-end in Gardner-only mode — the
        // pilot-aware AbsGardner only kicks in after the worker calls
        // `align_to_sof` (typically after `rx_v4_symbols` returned a
        // result and the worker can convert its SOF position to the
        // absolute symbol-index space).
        let sf = StreamingFrontend::new(profile_high_2x());
        assert!(sf.sof_anchor().is_none());
        assert_eq!(sf.symbols_emitted(), 0);
    }

    #[test]
    fn align_to_sof_enables_pilot_interior_gate() {
        // With anchor at 0, the per-cycle pilot positions should match
        // the precomputed `cycle_pilot_map`. Test the strict-interior
        // predicate is consistent: every interior symbol is_strict OK,
        // every block boundary (first/last of each 36-sym block) is
        // ruled out, every data symbol is also ruled out.
        let cfg = profile_high_2x();
        let mut sf = StreamingFrontend::new(cfg.clone());
        sf.align_to_sof(0);
        let map = cycle_pilot_map(&cfg);
        let n = map.len();
        for k in 0..n {
            let abs = k as u64;
            let p_lookup = map[k];
            let p_method = sf.is_pilot_symbol(abs);
            assert_eq!(p_lookup, p_method, "pilot mismatch at {k}");
            if p_lookup {
                let prev = if k > 0 { map[k - 1] } else { false };
                let next = if k + 1 < n { map[k + 1] } else { false };
                let interior_expected = prev && next;
                let interior_actual = sf.is_strict_pilot_interior(abs);
                assert_eq!(
                    interior_actual, interior_expected,
                    "interior mismatch at pilot {k} (prev={prev} next={next})",
                );
            } else {
                // Non-pilot positions are never interior.
                assert!(!sf.is_strict_pilot_interior(abs), "{k} is not pilot");
            }
        }
    }

    /// Helper: stream `audio` through a StreamingFrontend, optionally
    /// pinning the SOF anchor. Returns the timing-recovered symbol stream.
    fn stream_with_optional_anchor(
        cfg: &ModemConfig2x,
        audio: &[f32],
        anchor: Option<u64>,
    ) -> (Vec<Complex64>, f64) {
        let mut sf = StreamingFrontend::new(cfg.clone());
        if let Some(a) = anchor {
            sf.align_to_sof(a);
        }
        let mut syms = Vec::new();
        const CHUNK: usize = 24_000;
        for i in (0..audio.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(audio.len());
            syms.extend(sf.process_chunk(&audio[i..end]));
        }
        (syms, sf.timing_loop().integ())
    }

    #[test]
    fn pilot_aided_at_100_ppm_apsk32_matches_gardner_only() {
        // At 100 ppm — the upper edge of plain Gardner's safe window —
        // BOTH pilot-aided (anchor pinned) AND Gardner-only paths must
        // decode the same payload cleanly on HighPlus2x. The pilot-aided
        // path's value-add is not at this rate (Gardner alone copes)
        // but the test pins the API contract: enabling the pilot-aided
        // gate must not break what already worked.
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(400, 0xCAFE);
        let audio = modulate_for(&cfg, &payload, 0x1234);
        let resampled = resample_linear(&audio, 1.0 + 100e-6);

        let (syms_gardner, integ_g) = stream_with_optional_anchor(&cfg, &resampled, None);
        let res_g = modem_core2x::rx_v4::rx_v4_symbols(&syms_gardner, &cfg)
            .expect("Gardner-only decode at 100 ppm");
        assert_eq!(res_g.data, payload, "Gardner-only payload mismatch");
        assert!(integ_g > 0.0, "Gardner-only integ for +100ppm should be +, got {integ_g}");

        let (syms_pilot, integ_p) =
            stream_with_optional_anchor(&cfg, &resampled, Some(0));
        let res_p = modem_core2x::rx_v4::rx_v4_symbols(&syms_pilot, &cfg)
            .expect("pilot-aided decode at 100 ppm");
        assert_eq!(res_p.data, payload, "pilot-aided payload mismatch");
        assert!(integ_p > 0.0, "pilot-aided integ for +100ppm should be +, got {integ_p}");
    }

    #[test]
    fn pilot_aided_noise_free_does_not_regress_high_plus_2x() {
        // Opting in to pilot-aided mode on a noise-free, drift-free burst
        // MUST still decode cleanly — i.e. the AbsGardner-on-pilot
        // updates don't introduce spurious drift when there is nothing
        // to correct.
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(500, 0xC0DE);
        let audio = modulate_for(&cfg, &payload, 0x9999);
        let (syms, integ) = stream_with_optional_anchor(&cfg, &audio, Some(0));
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg)
            .expect("noise-free pilot-aided decode");
        assert_eq!(res.data, payload);
        // integ should stay small (no drift to track). The bound is
        // loose to absorb a few AbsGardner samples worth of noise — what
        // matters is that integ doesn't run off.
        assert!(integ.abs() < 1e-2, "integ wandered noise-free: {integ}");
    }

    #[test]
    fn pilot_aided_noise_free_apsk64() {
        // Same regression guard on the harshest multi-ring case (64-APSK)
        // — the pilot-aided gate must remain inert when there is no drift
        // even on the constellation that gave plain-Gardner the most
        // trouble historically.
        let cfg = profile_high_plus_plus_2x();
        let payload = rng_bytes(400, 0xC0DE);
        let audio = modulate_for(&cfg, &payload, 0x9999);
        let (syms, integ) = stream_with_optional_anchor(&cfg, &audio, Some(0));
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg)
            .expect("noise-free pilot-aided APSK64 decode");
        assert_eq!(res.data, payload);
        assert!(integ.abs() < 1e-2, "APSK64 integ wandered: {integ}");
    }

    #[test]
    fn pilot_aided_all_apsk_profiles_decode_at_50_ppm() {
        // Sweep all five APSK profiles at +50 ppm with the pilot anchor
        // explicitly set. Acts as a regression guard against future TED
        // changes that quietly break one corner of the constellation map.
        // 50 ppm is the well-trodden ground (the existing Gardner-only
        // test runs at exactly that) — we verify the pilot-aided gate
        // doesn't perturb what already worked.
        let apsk_profiles = [
            ProfileIndex2x::High2x,           // 16-APSK
            ProfileIndex2x::HighPlus2x,       // 32-APSK
            ProfileIndex2x::HighPlusPlus2x,   // 64-APSK
            ProfileIndex2x::HighFiveSix2x,    // 16-APSK rate 5/6
            ProfileIndex2x::HighPlusFiveSix2x,// 32-APSK rate 5/6
        ];
        for p in apsk_profiles {
            let cfg = p.to_config();
            let payload = rng_bytes(300, p.as_u8() as u64);
            let audio = modulate_for(&cfg, &payload, 0x42);
            let resampled = resample_linear(&audio, 1.0 + 50e-6);
            let (syms, _integ) =
                stream_with_optional_anchor(&cfg, &resampled, Some(0));
            let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg)
                .unwrap_or_else(|| panic!("{p:?} 50ppm pilot-aided decode None"));
            assert_eq!(res.data, payload, "{p:?} 50ppm pilot-aided payload");
        }
    }
}
