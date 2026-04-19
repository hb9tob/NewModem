//! Phase-A streaming v2 receiver — explicit state-machine skeleton.
//!
//! Complements `rx_stream.rs` with a more structured lifecycle that the GUI
//! can drive. Purely additive: reuses the existing preamble scan, symbol
//! extraction, marker decode and rx_v2 batch decoder; does not modify any
//! OTA-validated module.
//!
//! Lifecycle:
//! ```text
//!   Idle
//!     └─ preamble detected ─→ LockedTraining
//!   LockedTraining
//!     ├─ header decoded OK ─→ Streaming (emit HeaderDecoded)
//!     └─ too many failed attempts → SessionEnd{AbortTimeout}, back to Idle
//!   Streaming
//!     ├─ marker decoded (new seg_id) → emit MarkerSynced, stay
//!     ├─ silence ≥ T_silence_trigger, batch decode converges cleanly
//!     │      → emit FileComplete + SessionEnd{Completed}, back to Idle
//!     ├─ silence ≥ T_silence_trigger, batch decode not clean
//!     │      → emit SignalLost, go to LostSignal
//!     └─ session hard limit reached → force SessionEnd (Abort/Complete)
//!   LostSignal  ↔  Streaming
//!     ├─ signal returns within T_abort → emit SignalReacquired, back to
//!     │                                    Streaming
//!     └─ continuous silence ≥ T_abort (= 15 s) → emit SessionEnd{AbortTimeout}
//! ```
//!
//! Marker detection in Streaming is best-effort and throttled. The final
//! payload decode always runs via `rx_v2::rx_v2` on the buffered audio, so
//! robustness against squelch gaps and drift comes "for free" from the
//! existing pipeline.

use std::collections::VecDeque;

use crate::demodulator;
use crate::ffe;
use crate::header::{self, Header};
use crate::marker::{self, MarkerPayload, MARKER_LEN};
use crate::payload_envelope::PayloadEnvelope;
use crate::preamble;
use crate::profile::ProfileIndex;
use crate::rrc::{self, rrc_taps};
use crate::rx_v2::{self, RxV2Result};
use crate::rx_v2_stream::StreamingDecoder;
use crate::sync;
use crate::types::{Complex64, AUDIO_RATE, N_PREAMBLE, RRC_SPAN_SYM};

/// T_abort : continuous silence after SignalLost before we give up.
pub const T_ABORT_SEC: f64 = 15.0;

/// How long of silence inside Streaming before we try to finalise the decode.
const T_SILENCE_TRIGGER_SEC: f64 = 0.5;

/// RMS threshold under which a chunk is treated as silent.
const SILENCE_RMS_THRESHOLD: f32 = 0.005;

/// In Idle, keep at most this long a sliding window for preamble search.
const IDLE_WINDOW_SEC: f64 = 3.0;

/// Minimum buffer before attempting a preamble scan.
const MIN_BUFFER_FOR_SCAN: usize = AUDIO_RATE as usize; // 1 s

/// Idle preamble-scan cadence.
const IDLE_SCAN_EVERY_SEC: f64 = 1.0;

/// Marker re-check cadence once Streaming is active.
const MARKER_CHECK_EVERY_SEC: f64 = 2.0;

/// Progress-tick cadence inside Streaming — re-runs the batch decoder on the
/// current buffer and emits deltas (ProgressUpdate, early AppHeaderDecoded).
/// Not so aggressive that we stall the worker thread, not so slow that the
/// progress bar feels stale. 3 s is a reasonable middle ground for the
/// skeleton; a future phase will replace this with true incremental decode.
const PROGRESS_TICK_EVERY_SEC: f64 = 3.0;

/// Header decode attempt cadence inside LockedTraining.
const TRAINING_ATTEMPT_EVERY_SEC: f64 = 0.5;

/// Max header-decode attempts before LockedTraining gives up. Kept low so a
/// false preamble lock (VOX tone, payload sliver scoring above the detection
/// threshold) is discarded in ~2 s instead of 6 s — long enough to catch
/// genuine preambles that land slightly before the buffer holds enough data,
/// short enough that we return to Idle in time to catch a real preamble
/// trailing the false one.
const MAX_TRAINING_ATTEMPTS: usize = 4;

/// Max total session duration before we force-finalise (safety net).
const MAX_SESSION_SEC: u64 = 25 * 60;

/// Reason a session was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndReason {
    /// Full payload recovered (all blocks converged, hash match).
    Completed,
    /// T_abort elapsed without signal return, or header never decoded.
    AbortTimeout,
}

/// Events emitted by `StreamReceiverV2::feed`.
#[derive(Debug, Clone)]
pub enum StreamEventV2 {
    /// A preamble was detected; tentative profile lock (may be refined by the
    /// header's `profile_index`).
    PreambleDetected { profile: ProfileIndex },

    /// The protocol header was decoded. Authoritative profile is now known.
    HeaderDecoded {
        profile: ProfileIndex,
        mode_code: u8,
        payload_length: u16,
    },

    /// A new resync marker (by `seg_id`) was decoded mid-stream.
    MarkerSynced {
        seg_id: u16,
        base_esi: u32,
        is_meta: bool,
    },

    /// Signal stopped inside an active transmission (transitional).
    SignalLost,

    /// Signal returned before T_abort elapsed.
    SignalReacquired,

    /// AppHeader (session-meta) was recovered. May fire mid-stream (as soon
    /// as the meta segment converges) or at finalise time.
    AppHeaderDecoded {
        session_id: u32,
        file_size: u32,
        mime_type: u8,
        hash_short: u16,
    },

    /// Incremental decode progress. Fired on each tick where the number of
    /// converged LDPC blocks changed since the previous ProgressUpdate.
    /// `blocks_total` is what rx_v2 ATTEMPTED on this tick (may be less than
    /// `blocks_expected` if later segments haven't arrived yet).
    /// `blocks_expected` is derived from the protocol header's payload length
    /// and the profile's LDPC rate — ratio converged/expected is the true
    /// progress % the GUI should display.
    ///
    /// `converged_bitmap` is a bit-per-ESI mask (length `(blocks_expected +
    /// 7) / 8`), LSB-first within each byte, so the GUI can paint the
    /// per-block progress bar. `constellation_sample` is a small sample of
    /// the most recent main-modulation data symbols (after pilot-aided
    /// correction), for the live constellation view.
    ProgressUpdate {
        blocks_converged: usize,
        blocks_total: usize,
        blocks_expected: usize,
        sigma2: f64,
        converged_bitmap: Vec<u8>,
        constellation_sample: Vec<[f32; 2]>,
    },

    /// Payload envelope unwrapped.
    EnvelopeDecoded { filename: String, callsign: String },

    /// Payload fully decoded and hash-validated.
    FileComplete {
        filename: String,
        callsign: String,
        mime_type: u8,
        content: Vec<u8>,
        sigma2: f64,
    },

    /// The session is closed. The receiver is back to Idle.
    SessionEnd { reason: SessionEndReason },
}

/// Internal state machine.
#[derive(Debug)]
enum State {
    Idle,
    LockedTraining {
        profile: ProfileIndex,
        preamble_abs_offset: u64,
        attempts: usize,
        last_attempt_at_abs: u64,
    },
    Streaming {
        profile: ProfileIndex,
        preamble_abs_offset: u64,
        _header: Header,
        silence_samples: usize,
        last_marker_seg_id: Option<u16>,
        last_marker_check_abs: u64,
    },
    LostSignal {
        profile: ProfileIndex,
        preamble_abs_offset: u64,
        header: Header,
        /// Silence samples accumulated since the signal was lost.
        abort_accum_samples: usize,
    },
}

/// Live v2 receiver with explicit state machine.
pub struct StreamReceiverV2 {
    buffer: VecDeque<f32>,
    max_buffer_samples: usize,
    total_samples_fed: u64,
    state: State,
    last_idle_scan_at_abs: u64,
    idle_scan_every_samples: u64,
    training_attempt_every_samples: u64,
    marker_check_every_samples: u64,
    silence_trigger_samples: usize,
    abort_samples: usize,
    session_limit_samples: usize,

    // Per-session progress state (reset whenever we return to Idle).
    progress_tick_every_samples: u64,
    last_progress_tick_at_abs: u64,
    last_progress_emitted: Option<(usize, usize)>,
    app_header_emitted: bool,

    // Incremental decoder instantiated at HeaderDecoded, retired at SessionEnd.
    // While present, every new audio feed is mirrored to it (cheap, O(new
    // samples)) and the progress-tick / early-complete paths read its state
    // directly instead of re-running `rx_v2::rx_v2` on the whole buffer.
    streaming_decoder: Option<Box<StreamingDecoder>>,
    /// Absolute sample index up to which the streaming decoder has been fed.
    /// Diff against `total_samples_fed` gives the tail that still needs to
    /// be forwarded — see `feed()`.
    decoder_fed_up_to_abs: u64,
}

impl Default for StreamReceiverV2 {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamReceiverV2 {
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::with_capacity((30 * AUDIO_RATE) as usize),
            max_buffer_samples: (MAX_SESSION_SEC as usize) * AUDIO_RATE as usize,
            total_samples_fed: 0,
            state: State::Idle,
            last_idle_scan_at_abs: 0,
            idle_scan_every_samples: (IDLE_SCAN_EVERY_SEC * AUDIO_RATE as f64) as u64,
            training_attempt_every_samples: (TRAINING_ATTEMPT_EVERY_SEC * AUDIO_RATE as f64) as u64,
            marker_check_every_samples: (MARKER_CHECK_EVERY_SEC * AUDIO_RATE as f64) as u64,
            silence_trigger_samples: (T_SILENCE_TRIGGER_SEC * AUDIO_RATE as f64) as usize,
            abort_samples: (T_ABORT_SEC * AUDIO_RATE as f64) as usize,
            session_limit_samples: (MAX_SESSION_SEC as usize) * AUDIO_RATE as usize,
            progress_tick_every_samples: (PROGRESS_TICK_EVERY_SEC * AUDIO_RATE as f64) as u64,
            last_progress_tick_at_abs: 0,
            last_progress_emitted: None,
            app_header_emitted: false,
            streaming_decoder: None,
            decoder_fed_up_to_abs: 0,
        }
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.state = State::Idle;
        self.cleanup_session_state();
    }

    /// Called whenever we transition into Idle at the end of a session —
    /// scrubs per-session bookkeeping so the next session starts fresh.
    fn cleanup_session_state(&mut self) {
        self.last_idle_scan_at_abs = self.total_samples_fed;
        self.last_progress_tick_at_abs = self.total_samples_fed;
        self.last_progress_emitted = None;
        self.app_header_emitted = false;
        self.streaming_decoder = None;
        self.decoder_fed_up_to_abs = self.total_samples_fed;
    }

    pub fn is_locked(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    pub fn total_samples_fed(&self) -> u64 {
        self.total_samples_fed
    }

    /// Ingest a chunk of audio samples. Returns any events produced.
    pub fn feed(&mut self, samples: &[f32]) -> Vec<StreamEventV2> {
        // Append to bounded buffer
        for &s in samples {
            if self.buffer.len() >= self.max_buffer_samples {
                self.buffer.pop_front();
            }
            self.buffer.push_back(s);
        }
        self.total_samples_fed += samples.len() as u64;

        // Mirror the newly-arrived tail to the streaming decoder if one is
        // active. On the feed that CREATES the decoder (in `step_training`),
        // it is seeded inline with a buffer slice up to `total_samples_fed`
        // AND `decoder_fed_up_to_abs` is set to that same value — so this
        // pre-step forwarding is a no-op on that cycle and a full forward
        // on every subsequent feed. Cost is O(samples.len()).
        if self.streaming_decoder.is_some() {
            let diff = self
                .total_samples_fed
                .saturating_sub(self.decoder_fed_up_to_abs) as usize;
            if diff > 0 {
                let start = samples.len().saturating_sub(diff);
                if let Some(d) = self.streaming_decoder.as_mut() {
                    d.feed_samples(&samples[start..]);
                }
                self.decoder_fed_up_to_abs = self.total_samples_fed;
            }
        }

        let chunk_is_silent = compute_rms(samples) < SILENCE_RMS_THRESHOLD;

        // Drain-and-replace pattern so we can freely mutate state while the
        // match arm holds ownership of the variant fields.
        let mut events = Vec::new();
        let state = std::mem::replace(&mut self.state, State::Idle);
        self.state = self.step(state, samples, chunk_is_silent, &mut events);
        events
    }

    fn step(
        &mut self,
        state: State,
        samples: &[f32],
        chunk_is_silent: bool,
        events: &mut Vec<StreamEventV2>,
    ) -> State {
        match state {
            State::Idle => self.step_idle(events),
            State::LockedTraining {
                profile,
                preamble_abs_offset,
                attempts,
                last_attempt_at_abs,
            } => self.step_training(profile, preamble_abs_offset, attempts, last_attempt_at_abs, events),
            State::Streaming {
                profile,
                preamble_abs_offset,
                _header,
                silence_samples,
                last_marker_seg_id,
                last_marker_check_abs,
            } => self.step_streaming(
                profile,
                preamble_abs_offset,
                _header,
                silence_samples,
                last_marker_seg_id,
                last_marker_check_abs,
                samples.len(),
                chunk_is_silent,
                events,
            ),
            State::LostSignal {
                profile,
                preamble_abs_offset,
                header,
                abort_accum_samples,
            } => self.step_lost(
                profile,
                preamble_abs_offset,
                header,
                abort_accum_samples,
                samples.len(),
                chunk_is_silent,
                events,
            ),
        }
    }

    fn step_idle(&mut self, events: &mut Vec<StreamEventV2>) -> State {
        // Trim buffer to idle window so preamble scan stays O(1) per tick.
        let idle_window = (IDLE_WINDOW_SEC * AUDIO_RATE as f64) as usize;
        while self.buffer.len() > idle_window {
            self.buffer.pop_front();
        }
        let should_scan = self
            .total_samples_fed
            .saturating_sub(self.last_idle_scan_at_abs)
            >= self.idle_scan_every_samples
            && self.buffer.len() >= MIN_BUFFER_FOR_SCAN;
        if !should_scan {
            return State::Idle;
        }
        self.last_idle_scan_at_abs = self.total_samples_fed;

        // Silence skip: compute buffer RMS (one O(N) pass, cheap compared to
        // the 5× downmix + MF + find_preamble + sharpness that scan_preamble
        // would do). When the rig is parked on receive with no signal, the
        // RMS sits orders of magnitude below any real modem signal and we
        // skip the expensive scan entirely. This is what keeps the worker
        // thread from pinning the core between transmissions.
        let sum_sq: f32 = self.buffer.iter().map(|&s| s * s).sum();
        let rms = (sum_sq / self.buffer.len() as f32).sqrt();
        if rms < SILENCE_RMS_THRESHOLD {
            return State::Idle;
        }

        let Some((profile, offset_in_buffer)) = scan_preamble_multi_profile(&self.buffer) else {
            return State::Idle;
        };
        let preamble_abs_offset =
            self.total_samples_fed - self.buffer.len() as u64 + offset_in_buffer as u64;
        events.push(StreamEventV2::PreambleDetected { profile });
        State::LockedTraining {
            profile,
            preamble_abs_offset,
            attempts: 0,
            last_attempt_at_abs: 0,
        }
    }

    fn step_training(
        &mut self,
        profile: ProfileIndex,
        preamble_abs_offset: u64,
        attempts: usize,
        last_attempt_at_abs: u64,
        events: &mut Vec<StreamEventV2>,
    ) -> State {
        // Throttle attempts so we don't burn CPU rerunning FFE every 10 ms.
        let since_last = self
            .total_samples_fed
            .saturating_sub(last_attempt_at_abs);
        if since_last < self.training_attempt_every_samples && attempts > 0 {
            return State::LockedTraining {
                profile,
                preamble_abs_offset,
                attempts,
                last_attempt_at_abs,
            };
        }

        let slice = buffer_slice_from(&self.buffer, preamble_abs_offset, self.total_samples_fed);
        // Only the first attempt pays the multi-profile discrimination cost.
        // Subsequent attempts stay cheap (single-profile retry) because the
        // only reason to retry is "not enough data yet" for the tentative
        // profile — decode-failure-due-to-wrong-profile would already have
        // been exposed by the first multi-profile pass.
        let header_result = if attempts == 0 {
            try_decode_header_any(&slice, profile)
        } else {
            try_decode_header(&slice, profile).map(|h| (profile, h))
        };
        match header_result {
            Some((used_profile, hdr)) => {
                // Prefer profile_index WHEN it's self-consistent with the
                // header's mode_code. Early TX builds left profile_index at 0
                // (Ultra) regardless of the real profile — trusting it there
                // points the streaming decoder at the wrong pipeline and no
                // markers decode. Fall back to mode_code + used_profile when
                // the two disagree; HIGH / MEGA share a mode_code so in that
                // ambiguous case we use used_profile (the profile whose FFE
                // actually decoded the header), defaulting to HIGH if neither
                // matches.
                let authoritative = resolve_authoritative_profile(&hdr, used_profile);
                events.push(StreamEventV2::HeaderDecoded {
                    profile: authoritative,
                    mode_code: hdr.mode_code,
                    payload_length: hdr.payload_length,
                });
                // Start the progress-tick clock at Streaming entry — first
                // tick fires PROGRESS_TICK_EVERY_SEC after lock, not
                // immediately (rx_v2 on a ~1 s buffer would be pointless).
                self.last_progress_tick_at_abs = self.total_samples_fed;

                // Spin up the incremental decoder and seed it with every
                // buffered sample since the preamble. Subsequent feeds will
                // forward only the new tail via `feed()` (see there). The
                // seed absorbs the one-time cost of running the FFE LS +
                // header decode inside the streaming decoder; after that
                // the per-feed cost is bounded by the feed size.
                let mut decoder = Box::new(StreamingDecoder::new(authoritative));
                let seed = buffer_slice_from(
                    &self.buffer,
                    preamble_abs_offset,
                    self.total_samples_fed,
                );
                decoder.feed_samples(&seed);
                self.streaming_decoder = Some(decoder);
                self.decoder_fed_up_to_abs = self.total_samples_fed;

                State::Streaming {
                    profile: authoritative,
                    preamble_abs_offset,
                    _header: hdr,
                    silence_samples: 0,
                    last_marker_seg_id: None,
                    last_marker_check_abs: 0,
                }
            }
            None => {
                // Fast path: the first attempt has enough data for every
                // profile (including the slowest — Ultra needs ~704 ms for
                // preamble+header) yet NO profile produced a valid header
                // → definitely a false lock. Skip the retry loop, abort now.
                // ULTRA_HEADER_SAMPLES ≈ 0.8 × 48_000 covers (N_PREAMBLE + 96)
                // Ultra symbols with margin.
                const ULTRA_HEADER_SAMPLES: usize = 38_400; // 0.8 s
                if attempts == 0 && slice.len() >= ULTRA_HEADER_SAMPLES {
                    events.push(StreamEventV2::SessionEnd {
                        reason: SessionEndReason::AbortTimeout,
                    });
                    self.cleanup_session_state();
                    self.last_idle_scan_at_abs = self
                        .total_samples_fed
                        .saturating_sub(self.idle_scan_every_samples);
                    return State::Idle;
                }
                let next_attempts = attempts + 1;
                if next_attempts >= MAX_TRAINING_ATTEMPTS {
                    // Couldn't lock onto a valid header — treat as abort.
                    // IMPORTANT: do NOT clear the buffer. A real preamble may
                    // have landed in the buffer while we were falsely locked
                    // on VOX/noise; clearing here would drop it and leave the
                    // real TX undetected. Idle's own sliding-window cap
                    // (IDLE_WINDOW_SEC) will trim it on the next tick.
                    events.push(StreamEventV2::SessionEnd {
                        reason: SessionEndReason::AbortTimeout,
                    });
                    self.cleanup_session_state();
                    // Reset the Idle scan clock so we re-scan immediately,
                    // without waiting the usual 1 s cadence — the preamble
                    // we may have missed is about to slide out.
                    self.last_idle_scan_at_abs = self
                        .total_samples_fed
                        .saturating_sub(self.idle_scan_every_samples);
                    State::Idle
                } else {
                    State::LockedTraining {
                        profile,
                        preamble_abs_offset,
                        attempts: next_attempts,
                        last_attempt_at_abs: self.total_samples_fed,
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step_streaming(
        &mut self,
        profile: ProfileIndex,
        preamble_abs_offset: u64,
        header: Header,
        mut silence_samples: usize,
        mut last_marker_seg_id: Option<u16>,
        last_marker_check_abs: u64,
        chunk_len: usize,
        chunk_is_silent: bool,
        events: &mut Vec<StreamEventV2>,
    ) -> State {
        if chunk_is_silent {
            silence_samples += chunk_len;
        } else {
            silence_samples = 0;
        }

        // Drain markers the streaming decoder has picked up since last feed.
        // Markers surface naturally from the decoder's segment walker, so we
        // no longer need the standalone `detect_latest_marker` best-effort
        // scan that used to run on a throttled cadence.
        if let Some(d) = self.streaming_decoder.as_mut() {
            for mk in d.drain_new_markers() {
                if Some(mk.seg_id) != last_marker_seg_id {
                    last_marker_seg_id = Some(mk.seg_id);
                    events.push(StreamEventV2::MarkerSynced {
                        seg_id: mk.seg_id,
                        base_esi: mk.base_esi,
                        is_meta: mk.is_meta(),
                    });
                }
            }
        }

        // Progress tick : read the streaming decoder's live counters and
        // surface deltas. Cheap (O(1) state reads) so we could fire every
        // feed — keeping the existing cadence just avoids event spam.
        let should_tick = self
            .total_samples_fed
            .saturating_sub(self.last_progress_tick_at_abs)
            >= self.progress_tick_every_samples;
        if should_tick {
            self.last_progress_tick_at_abs = self.total_samples_fed;
            if let Some(d) = self.streaming_decoder.as_ref() {
                if !self.app_header_emitted {
                    if let Some(ah) = d.app_header() {
                        self.app_header_emitted = true;
                        events.push(StreamEventV2::AppHeaderDecoded {
                            session_id: ah.session_id,
                            file_size: ah.file_size,
                            mime_type: ah.mime_type,
                            hash_short: ah.hash_short,
                        });
                    }
                }
                let data_recovered = d.data_blocks_recovered();
                let expected = d.expected_data_blocks();
                if expected > 0 {
                    let now = (data_recovered, expected);
                    if self.last_progress_emitted != Some(now) {
                        self.last_progress_emitted = Some(now);
                        events.push(StreamEventV2::ProgressUpdate {
                            blocks_converged: data_recovered,
                            blocks_total: d.total_blocks(),
                            blocks_expected: expected,
                            sigma2: d.sigma2(),
                            converged_bitmap: d.converged_esi_bitmap(expected),
                            constellation_sample: d.recent_data_syms(),
                        });
                    }
                }
            }

            // Early FileComplete: decoder has a valid AppHeader + every data
            // block + the content hash matches. Fire the full completion path
            // without waiting for silence.
            if let Some(mut fe) = self.try_early_complete_from_decoder() {
                events.append(&mut fe);
                events.push(StreamEventV2::SessionEnd {
                    reason: SessionEndReason::Completed,
                });
                self.buffer.clear();
                self.cleanup_session_state();
                return State::Idle;
            }
        }

        let session_duration = self
            .total_samples_fed
            .saturating_sub(preamble_abs_offset) as usize;
        let hit_hard_limit = session_duration >= self.session_limit_samples;

        if silence_samples >= self.silence_trigger_samples || hit_hard_limit {
            // Finalise: prefer the streaming decoder's own assembly (cheap,
            // reflects everything we've decoded live). If it can't produce a
            // clean decode, fall back to the batch `rx_v2::rx_v2` which still
            // carries the ppm-drift grid search — the one-shot cost there is
            // acceptable because it only runs once per session.
            let outcome = self.finalise_outcome(preamble_abs_offset, profile);
            match outcome {
                FinaliseOutcome::Clean(mut fe) => {
                    events.append(&mut fe);
                    events.push(StreamEventV2::SessionEnd {
                        reason: SessionEndReason::Completed,
                    });
                    self.buffer.clear();
                    self.cleanup_session_state();
                    return State::Idle;
                }
                FinaliseOutcome::Partial(_) => {
                    if hit_hard_limit {
                        events.push(StreamEventV2::SessionEnd {
                            reason: SessionEndReason::AbortTimeout,
                        });
                        self.buffer.clear();
                        self.cleanup_session_state();
                        return State::Idle;
                    }
                    events.push(StreamEventV2::SignalLost);
                    return State::LostSignal {
                        profile,
                        preamble_abs_offset,
                        header,
                        abort_accum_samples: silence_samples,
                    };
                }
            }
        }

        State::Streaming {
            profile,
            preamble_abs_offset,
            _header: header,
            silence_samples,
            last_marker_seg_id,
            last_marker_check_abs,
        }
    }

    /// If the streaming decoder has recovered every data block and the
    /// content hash matches the AppHeader, build the clean finalise-path
    /// events. Returns `None` otherwise (still in progress).
    fn try_early_complete_from_decoder(&self) -> Option<Vec<StreamEventV2>> {
        let d = self.streaming_decoder.as_ref()?;
        let ah = d.app_header()?;
        if !d.all_data_present() {
            return None;
        }
        let assembled = d.try_assemble()?;
        if fnv1a_u16(&assembled) != ah.hash_short {
            return None;
        }
        Some(clean_events_from_assembled(assembled, ah, d.sigma2()))
    }

    /// End-of-session decode driven purely from the streaming decoder state —
    /// bounded CPU, no multi-minute grid-search fallback that would freeze the
    /// live worker on long marginal OTA captures.
    ///
    /// If the decoder recovered every data block and the content hash matches
    /// the AppHeader, return `Clean` (FileComplete will fire). Otherwise
    /// surface whatever metadata we have (AppHeaderDecoded at most) as
    /// `Partial`. Users with marginal captures can re-decode offline via the
    /// CLI's `rx` subcommand, which keeps the drift-aware grid search.
    fn finalise_outcome(&self, _preamble_abs_offset: u64, _profile: ProfileIndex) -> FinaliseOutcome {
        let Some(d) = self.streaming_decoder.as_ref() else {
            return FinaliseOutcome::Partial(Vec::new());
        };
        if let Some(ah) = d.app_header() {
            if let Some(assembled) = d.try_assemble() {
                if fnv1a_u16(&assembled) == ah.hash_short {
                    return FinaliseOutcome::Clean(clean_events_from_assembled(
                        assembled,
                        ah,
                        d.sigma2(),
                    ));
                }
            }
            // AppHeader known but content hash mismatch: emit the metadata so
            // the GUI can show what session was heard, but no FileComplete.
            let events = vec![StreamEventV2::AppHeaderDecoded {
                session_id: ah.session_id,
                file_size: ah.file_size,
                mime_type: ah.mime_type,
                hash_short: ah.hash_short,
            }];
            return FinaliseOutcome::Partial(events);
        }
        FinaliseOutcome::Partial(Vec::new())
    }

    fn step_lost(
        &mut self,
        profile: ProfileIndex,
        preamble_abs_offset: u64,
        header: Header,
        mut abort_accum_samples: usize,
        chunk_len: usize,
        chunk_is_silent: bool,
        events: &mut Vec<StreamEventV2>,
    ) -> State {
        if !chunk_is_silent {
            events.push(StreamEventV2::SignalReacquired);
            return State::Streaming {
                profile,
                preamble_abs_offset,
                _header: header,
                silence_samples: 0,
                last_marker_seg_id: None,
                last_marker_check_abs: self.total_samples_fed,
            };
        }
        abort_accum_samples += chunk_len;
        if abort_accum_samples >= self.abort_samples {
            let outcome = self.finalise_outcome(preamble_abs_offset, profile);
            let (mut final_events, reason) = match outcome {
                FinaliseOutcome::Clean(fe) => (fe, SessionEndReason::Completed),
                FinaliseOutcome::Partial(fe) => (fe, SessionEndReason::AbortTimeout),
            };
            events.append(&mut final_events);
            events.push(StreamEventV2::SessionEnd { reason });
            self.buffer.clear();
            self.cleanup_session_state();
            return State::Idle;
        }
        State::LostSignal {
            profile,
            preamble_abs_offset,
            header,
            abort_accum_samples,
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

fn buffer_slice_from(buffer: &VecDeque<f32>, abs_offset: u64, total_samples_fed: u64) -> Vec<f32> {
    let buffer_start_abs = total_samples_fed - buffer.len() as u64;
    let skip = abs_offset.saturating_sub(buffer_start_abs) as usize;
    buffer.iter().skip(skip).copied().collect()
}

/// Correlate the preamble pattern against `mf` at position `start` with
/// `pitch`-sample spacing. Returns |Σ mf[pos+k*pitch] · preamble[k]*|².
fn preamble_corr_mag2(
    mf: &[Complex64],
    preamble_syms: &[Complex64],
    start: usize,
    pitch: usize,
) -> f64 {
    let mut acc = Complex64::new(0.0, 0.0);
    for (k, &sym) in preamble_syms.iter().enumerate() {
        let idx = start + k * pitch;
        if idx >= mf.len() {
            break;
        }
        acc += mf[idx] * sym.conj();
    }
    acc.norm_sqr()
}

/// Peak-to-sidelobe sharpness of the preamble correlation at `center_pos`.
/// CAZAC-like preambles peak at N² at the correct alignment and drop to ~N
/// at any other symbol-grid offset — ratio ~ √N_PREAMBLE = 16 in clean
/// conditions. Correlations on voice / VOX tone / payload have no such peak
/// and score near 1.0. `None` if too close to the buffer edges to measure
/// sidelobes (caller should treat that as not-sharp-enough).
fn preamble_sharpness(
    mf: &[Complex64],
    preamble_syms: &[Complex64],
    center_pos: usize,
    pitch: usize,
) -> Option<f64> {
    // Center correlation
    let center = preamble_corr_mag2(mf, preamble_syms, center_pos, pitch);
    // Sidelobe correlations at ±k symbols, k in {1,2,3,4,6,8}. Picking a
    // spread of offsets (not just neighbours) prevents a partial-overlap
    // artefact from inflating the sidelobe mean.
    let offsets: [isize; 12] = [-8, -6, -4, -3, -2, -1, 1, 2, 3, 4, 6, 8];
    let mut sum = 0.0;
    let mut n = 0usize;
    let end_ok = mf.len().saturating_sub(N_PREAMBLE * pitch);
    for d in offsets {
        let shifted = center_pos as isize + d * pitch as isize;
        if shifted < 0 || (shifted as usize) > end_ok {
            continue;
        }
        sum += preamble_corr_mag2(mf, preamble_syms, shifted as usize, pitch);
        n += 1;
    }
    if n < 4 {
        // Not enough sidelobes to trust the ratio (buffer too short or pos
        // too close to edges).
        return None;
    }
    let side_mean = (sum / n as f64).max(1e-12);
    Some(center / side_mean)
}

/// Multi-profile preamble scan against the live sliding buffer. Accepts a
/// candidate only if its peak/sidelobe ratio is well above 1 (CAZAC peak
/// property), which rejects correlations on voice / VOX tone / payload
/// regardless of their absolute amplitude. Deliberately permissive on the
/// absolute score so we still catch faint-but-real preambles.
fn scan_preamble_multi_profile(buffer: &VecDeque<f32>) -> Option<(ProfileIndex, usize)> {
    let samples: Vec<f32> = buffer.iter().copied().collect();
    let preamble_syms = preamble::make_preamble();

    // All predefined profiles share fc=DATA_CENTER_HZ, so downmix once.
    // NORMAL / HIGH / MEGA additionally share (beta, sps)=(0.20, 32) → same
    // RRC taps → same matched-filter output (only `pitch` differs for the
    // search). Cache MF by (beta_millis, sps) to cut the per-scan MF count
    // from 5 to 3 unique computations on the default profile set.
    let bb = demodulator::downmix(&samples, crate::types::DATA_CENTER_HZ);
    let mut mf_cache: Vec<((u32, usize), Vec<Complex64>)> = Vec::with_capacity(3);

    let mut best: Option<(ProfileIndex, usize, f64)> = None;
    for &profile in &ProfileIndex::ALL {
        let cfg = profile.to_config();
        let (sps, pitch) = match rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
        {
            Ok(x) => x,
            Err(_) => continue,
        };
        let key = ((cfg.beta * 1000.0).round() as u32, sps);
        let mf: &[Complex64] = if let Some(entry) = mf_cache.iter().find(|(k, _)| *k == key) {
            entry.1.as_slice()
        } else {
            let taps = rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
            mf_cache.push((key, demodulator::matched_filter(&bb, &taps)));
            mf_cache.last().unwrap().1.as_slice()
        };
        let pos = match sync::find_preamble(mf, sps, pitch, cfg.beta) {
            Some(p) => p,
            None => continue,
        };
        let Some(sharpness) = preamble_sharpness(mf, &preamble_syms, pos, pitch) else {
            continue;
        };
        match best {
            None => best = Some((profile, pos, sharpness)),
            Some((_, _, prev)) if sharpness > prev => best = Some((profile, pos, sharpness)),
            _ => {}
        }
    }
    // Sharpness threshold: real preambles in clean OTA score >> 30; VOX
    // tones, voice, squelch all hover near 1–12. 15 leaves comfortable
    // margin on both sides.
    best.and_then(|(p, pos, sharpness)| if sharpness > 15.0 { Some((p, pos)) } else { None })
}

/// Decode only the protocol header from an audio slice (preamble + 96 header
/// symbols). Shares the FFE training prologue with `rx_v2_single`.
fn try_decode_header(samples: &[f32], profile: ProfileIndex) -> Option<Header> {
    let syms = extract_symbols_after_preamble(samples, profile, N_PREAMBLE + 96)?;
    header::decode_header_symbols(&syms[N_PREAMBLE..N_PREAMBLE + 96])
}

/// Try to decode the header with every known profile in turn. Order: the
/// caller's tentative guess first (fast-path for the common case), then the
/// rest. The header carries CRC8 + Golay(24,12) protection — a successful
/// decode that ALSO reports a valid `profile_index` byte is the strongest
/// possible discriminator (false-positive rate ~2⁻⁸ per profile tried).
/// Returns `(profile_used, header)` of the first profile that produces a
/// valid header, or `None` if every profile fails.
fn try_decode_header_any(
    samples: &[f32],
    tentative: ProfileIndex,
) -> Option<(ProfileIndex, Header)> {
    if let Some(h) = try_decode_header(samples, tentative) {
        return Some((tentative, h));
    }
    for &p in &ProfileIndex::ALL {
        if p == tentative {
            continue;
        }
        if let Some(h) = try_decode_header(samples, p) {
            return Some((p, h));
        }
    }
    None
}

/// Best-effort latest-marker detection. Runs the same FFE prologue and then
/// `marker::find_sync_in_window` across the post-header region.
fn detect_latest_marker(
    buffer: &VecDeque<f32>,
    preamble_abs_offset: u64,
    total_samples_fed: u64,
    profile: ProfileIndex,
) -> Option<MarkerPayload> {
    let slice = buffer_slice_from(buffer, preamble_abs_offset, total_samples_fed);
    let syms = extract_symbols_after_preamble(&slice, profile, usize::MAX)?;
    if syms.len() < N_PREAMBLE + 96 + MARKER_LEN {
        return None;
    }
    let data_region = &syms[N_PREAMBLE + 96..];
    let mut cursor = 0usize;
    let mut latest: Option<MarkerPayload> = None;
    while cursor + MARKER_LEN <= data_region.len() {
        let window = 64;
        let end = (cursor + window).min(data_region.len().saturating_sub(MARKER_LEN));
        if end <= cursor {
            break;
        }
        match marker::find_sync_in_window(data_region, cursor, end - cursor, 0.5) {
            Some((pos, _)) => {
                if let Some(p) = marker::decode_marker_at(&data_region[pos..pos + MARKER_LEN]) {
                    latest = Some(p);
                    cursor = pos + MARKER_LEN;
                } else {
                    cursor = pos + MARKER_LEN;
                }
            }
            None => {
                cursor += MARKER_LEN;
            }
        }
    }
    latest
}

/// Run the shared FFE prologue (same one `rx_v2_single` uses) and return up
/// to `max_symbols` equalised symbols starting at the preamble. Returns None
/// if the preamble can't be found or there aren't enough samples for even a
/// minimal FFE window.
fn extract_symbols_after_preamble(
    samples: &[f32],
    profile: ProfileIndex,
    max_symbols: usize,
) -> Option<Vec<Complex64>> {
    let cfg = profile.to_config();
    let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).ok()?;
    let taps = rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
    let bb = demodulator::downmix(samples, cfg.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);
    let sync_pos = sync::find_preamble(&mf, sps, pitch, cfg.beta)?;
    let (fse_input, fse_start, d_fse) = sync::decimate_for_fse(&mf, sync_pos, sps, pitch);
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
    let preamble_syms = preamble::make_preamble();
    let training_positions: Vec<usize> = (0..N_PREAMBLE)
        .map(|k| fse_start + k * pitch_fse)
        .collect();
    let ffe_initial = ffe::train_ffe_ls(&fse_input, &preamble_syms, &training_positions, n_ff);
    let half = n_ff / 2;
    let available = if fse_input.len() > fse_start + half {
        (fse_input.len() - fse_start - half) / pitch_fse + 1
    } else {
        0
    };
    if available < N_PREAMBLE + 96 {
        return None;
    }
    let wanted = max_symbols.min(available);
    let constellation = crate::frame::make_constellation(&cfg);
    let preamble_training: Vec<(usize, Complex64)> = preamble_syms
        .iter()
        .enumerate()
        .map(|(k, &s)| (k, s))
        .collect();
    let (all_rx_syms, _) = ffe::apply_ffe_lms_with_training(
        &fse_input,
        &ffe_initial,
        fse_start,
        pitch_fse,
        wanted,
        &preamble_training,
        &constellation,
        0.10,
        0.02,
    );
    if all_rx_syms.len() < N_PREAMBLE + 96 {
        return None;
    }
    // Global gain LS from preamble
    let mut num = Complex64::new(0.0, 0.0);
    let mut den = 0.0f64;
    for k in 0..N_PREAMBLE {
        num += all_rx_syms[k] * preamble_syms[k].conj();
        den += preamble_syms[k].norm_sqr();
    }
    let gain = if den > 1e-12 {
        num / den
    } else {
        Complex64::new(1.0, 0.0)
    };
    Some(all_rx_syms.iter().map(|&s| s / gain).collect())
}

enum FinaliseOutcome {
    /// Full payload recovered (hash match + all blocks converged).
    Clean(Vec<StreamEventV2>),
    /// Decode didn't fully converge — emit whatever metadata we got.
    Partial(Vec<StreamEventV2>),
}

/// Run `rx_v2::rx_v2` on a slice. Retries once with the header-authoritative
/// profile when the first pass disagrees with the caller's (resolves
/// NORMAL↔HIGH et al.). Shared between the progress tick and finalise paths
/// so we don't pay double rx_v2 cost per feed.
fn run_rx_v2_with_retry(slice: &[f32], profile: ProfileIndex) -> Option<RxV2Result> {
    let cfg = profile.to_config();
    let first = rx_v2::rx_v2(slice, &cfg)?;
    let auth = first
        .header
        .as_ref()
        .and_then(|h| ProfileIndex::from_u8(h.profile_index));
    match auth {
        Some(real_p) if real_p != profile => {
            let cfg2 = real_p.to_config();
            Some(rx_v2::rx_v2(slice, &cfg2).unwrap_or(first))
        }
        _ => Some(first),
    }
}

/// Translate an `RxV2Result` into the finalise-path events + clean/partial
/// classification. Used at silence-trigger or T_abort timeout.
fn finalise_events_from_result(result: RxV2Result) -> FinaliseOutcome {
    let mut events: Vec<StreamEventV2> = Vec::new();
    if let Some(ref ah) = result.app_header {
        events.push(StreamEventV2::AppHeaderDecoded {
            session_id: ah.session_id,
            file_size: ah.file_size,
            mime_type: ah.mime_type,
            hash_short: ah.hash_short,
        });
    }
    let hash_match = result
        .app_header
        .as_ref()
        .map(|a| fnv1a_u16(&result.data) == a.hash_short)
        .unwrap_or(false);
    // Content-hash match is the authoritative "decode is clean" gate. The
    // earlier additional `converged_blocks == total_blocks` check meant a
    // single meta-segment that failed LDPC convergence would veto the
    // FileComplete even though every data block was recovered and the
    // content hash matched — exactly the "105/105 but no image" failure.
    let fully_converged = hash_match;
    if fully_converged {
        let envelope = PayloadEnvelope::decode_or_fallback(&result.data);
        let mime = result.app_header.as_ref().map(|a| a.mime_type).unwrap_or(0);
        events.push(StreamEventV2::EnvelopeDecoded {
            filename: envelope.filename.clone(),
            callsign: envelope.callsign.clone(),
        });
        events.push(StreamEventV2::FileComplete {
            filename: envelope.filename,
            callsign: envelope.callsign,
            mime_type: mime,
            content: envelope.content,
            sigma2: result.sigma2,
        });
        FinaliseOutcome::Clean(events)
    } else {
        FinaliseOutcome::Partial(events)
    }
}

fn finalise_decode(slice: &[f32], profile: ProfileIndex) -> FinaliseOutcome {
    match run_rx_v2_with_retry(slice, profile) {
        Some(r) => finalise_events_from_result(r),
        None => FinaliseOutcome::Partial(Vec::new()),
    }
}

/// Map a decoded header to the most-likely `ProfileIndex`. Prefers the
/// header's `profile_index` byte when it's consistent with `mode_code`;
/// otherwise derives the profile from `mode_code` alone, using
/// `used_profile` (the one whose FFE decoded the header) as the tiebreaker
/// for the HIGH/MEGA ambiguity.
fn resolve_authoritative_profile(hdr: &Header, used_profile: ProfileIndex) -> ProfileIndex {
    if let Some(declared) = ProfileIndex::from_u8(hdr.profile_index) {
        if declared.to_config().mode_code() == hdr.mode_code {
            return declared;
        }
    }
    // profile_index absent / inconsistent — derive from mode_code.
    if used_profile.to_config().mode_code() == hdr.mode_code {
        return used_profile;
    }
    for &p in &ProfileIndex::ALL {
        if p.to_config().mode_code() == hdr.mode_code {
            return p;
        }
    }
    used_profile
}

/// Build the clean-path event sequence (AppHeaderDecoded, EnvelopeDecoded,
/// FileComplete) from an already-assembled payload that passed the hash gate.
/// Used by the streaming-decoder early-complete and finalise paths so they
/// don't have to round-trip through `rx_v2::rx_v2` just to produce events.
fn clean_events_from_assembled(
    assembled: Vec<u8>,
    ah: &crate::app_header::AppHeader,
    sigma2: f64,
) -> Vec<StreamEventV2> {
    let mut events: Vec<StreamEventV2> = Vec::new();
    events.push(StreamEventV2::AppHeaderDecoded {
        session_id: ah.session_id,
        file_size: ah.file_size,
        mime_type: ah.mime_type,
        hash_short: ah.hash_short,
    });
    let envelope = PayloadEnvelope::decode_or_fallback(&assembled);
    events.push(StreamEventV2::EnvelopeDecoded {
        filename: envelope.filename.clone(),
        callsign: envelope.callsign.clone(),
    });
    events.push(StreamEventV2::FileComplete {
        filename: envelope.filename,
        callsign: envelope.callsign,
        mime_type: ah.mime_type,
        content: envelope.content,
        sigma2,
    });
    events
}

/// Number of source-data LDPC blocks the TX would need to carry
/// `payload_length` bytes at `profile`'s code rate. Excludes meta-segment
/// blocks (those are framing overhead, not content).
fn expected_data_blocks(payload_length: u16, profile: ProfileIndex) -> usize {
    let cfg = profile.to_config();
    let k_bytes = cfg.ldpc_rate.k() / 8;
    (payload_length as usize + k_bytes - 1) / k_bytes
}

/// FNV-1a folded to 16 bits — matches the TX's content hash.
fn fnv1a_u16(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_header::mime;
    use crate::frame;
    use crate::modulator;
    use crate::profile::{profile_normal, ModemConfig};

    fn make_v2_tx(data: &[u8], config: &ModemConfig, session_id: u32) -> Vec<f32> {
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau).unwrap();
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let envelope = PayloadEnvelope::new("test.bin", "HB9TST", data.to_vec()).unwrap();
        let wire = envelope.encode();
        let hash = fnv1a_u16(&wire);
        let symbols =
            frame::build_superframe_v2(&wire, config, session_id, mime::BINARY, hash);
        modulator::modulate(&symbols, sps, pitch, &taps, config.center_freq_hz)
    }

    fn feed_all(rx: &mut StreamReceiverV2, samples: &[f32]) -> Vec<StreamEventV2> {
        let chunk = (AUDIO_RATE / 100) as usize; // 10 ms @ 48 kHz
        let mut events = Vec::new();
        for c in samples.chunks(chunk) {
            events.extend(rx.feed(c));
        }
        events
    }

    #[test]
    fn happy_path_normal() {
        let config = profile_normal();
        let data: Vec<u8> = (0..500).map(|i| (i * 11) as u8).collect();
        let tx = make_v2_tx(&data, &config, 0xBEEF_CAFE);

        let mut stream = Vec::new();
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 0.5) as usize]); // leading silence
        stream.extend_from_slice(&tx);
        // Enough trailing silence to cross T_silence_trigger and let finalise run.
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 1.2) as usize]);

        let mut rx = StreamReceiverV2::new();
        let events = feed_all(&mut rx, &stream);

        let preamble = events
            .iter()
            .any(|e| matches!(e, StreamEventV2::PreambleDetected { .. }));
        let header = events.iter().find_map(|e| match e {
            StreamEventV2::HeaderDecoded { profile, .. } => Some(*profile),
            _ => None,
        });
        let complete = events.iter().find_map(|e| match e {
            StreamEventV2::FileComplete { content, .. } => Some(content.clone()),
            _ => None,
        });
        let end_reason = events.iter().rev().find_map(|e| match e {
            StreamEventV2::SessionEnd { reason } => Some(*reason),
            _ => None,
        });

        assert!(preamble, "missing PreambleDetected");
        assert_eq!(header, Some(ProfileIndex::Normal));
        assert_eq!(complete.as_deref(), Some(&data[..]));
        assert_eq!(end_reason, Some(SessionEndReason::Completed));
        assert!(!rx.is_locked(), "should be back to Idle after SessionEnd");
    }

    #[test]
    fn squelch_mid_tx_transitions() {
        // A squelch gap in the middle of the TX. Phase A guarantees the state
        // machine emits SignalLost when silence crosses the trigger, then
        // SignalReacquired once the signal returns, and finally closes the
        // session. End-to-end payload recovery through the gap depends on
        // `rx_v2`'s marker tracking and is exercised in its own test suite —
        // we don't re-assert it here.
        let config = profile_normal();
        let data: Vec<u8> = (0..400).map(|i| (i * 7) as u8).collect();
        let tx = make_v2_tx(&data, &config, 0xFACE_BEEF);

        // 1 s of silence inside the TX: comfortably above T_silence_trigger
        // (0.5 s) and well below T_abort (15 s).
        let mut with_gap = tx.clone();
        let gap_start = with_gap.len() / 2;
        let gap_len = AUDIO_RATE as usize;
        for s in with_gap.iter_mut().skip(gap_start).take(gap_len) {
            *s = 0.0;
        }

        let mut stream = Vec::new();
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 0.5) as usize]);
        stream.extend_from_slice(&with_gap);
        // Enough trailing silence to let T_abort elapse after the post-TX
        // SignalLost → LostSignal transition.
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * (T_ABORT_SEC + 1.0)) as usize]);

        let mut rx = StreamReceiverV2::new();
        let events = feed_all(&mut rx, &stream);

        let n_lost = events
            .iter()
            .filter(|e| matches!(e, StreamEventV2::SignalLost))
            .count();
        let n_reacq = events
            .iter()
            .filter(|e| matches!(e, StreamEventV2::SignalReacquired))
            .count();
        let got_session_end = events
            .iter()
            .any(|e| matches!(e, StreamEventV2::SessionEnd { .. }));

        // During the gap we emit SignalLost; when the signal comes back,
        // SignalReacquired; after the TX ends and trailing silence exceeds
        // T_abort, SessionEnd closes the session.
        assert!(n_lost >= 1, "expected at least one SignalLost event");
        assert!(n_reacq >= 1, "expected at least one SignalReacquired event");
        assert!(got_session_end, "session must close after the gap + trailing silence");
        assert!(!rx.is_locked(), "should be back to Idle after SessionEnd");
    }

    #[test]
    fn truncated_tx_aborts_after_t_abort() {
        // Cut the TX at ~40 % of its length and feed trailing silence long
        // enough to trigger T_abort (15 s). Expect SignalLost + SessionEnd
        // {AbortTimeout}, and never FileComplete.
        let config = profile_normal();
        let data: Vec<u8> = (0..600).map(|i| (i * 13) as u8).collect();
        let tx = make_v2_tx(&data, &config, 0xDEAD_BEEF);

        let cut = tx.len() * 2 / 5;
        let mut stream = Vec::new();
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 0.5) as usize]);
        stream.extend_from_slice(&tx[..cut]);
        // T_abort = 15 s + small margin.
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 16.0) as usize]);

        let mut rx = StreamReceiverV2::new();
        let events = feed_all(&mut rx, &stream);

        let got_lost = events
            .iter()
            .any(|e| matches!(e, StreamEventV2::SignalLost));
        let got_complete = events
            .iter()
            .any(|e| matches!(e, StreamEventV2::FileComplete { .. }));
        let end_reason = events.iter().rev().find_map(|e| match e {
            StreamEventV2::SessionEnd { reason } => Some(*reason),
            _ => None,
        });

        assert!(got_lost, "expected SignalLost after truncation");
        assert!(!got_complete, "truncated TX must not produce FileComplete");
        assert_eq!(end_reason, Some(SessionEndReason::AbortTimeout));
        assert!(!rx.is_locked(), "should be back to Idle after abort");
    }

    #[test]
    fn pure_noise_never_completes() {
        // 6 s of low-amplitude white-ish noise. A momentary PreambleDetected
        // is allowed (false lock), but FileComplete must never fire.
        let mut rx = StreamReceiverV2::new();
        let mut state = 12345u64;
        let chunk_len = 4800;
        let mut chunk = vec![0.0f32; chunk_len];
        let mut got_complete = false;
        for _ in 0..60 {
            for s in chunk.iter_mut() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let b = (state >> 33) as u32;
                *s = (b as f32 / u32::MAX as f32 - 0.5) * 0.1;
            }
            for ev in rx.feed(&chunk) {
                if let StreamEventV2::FileComplete { .. } = ev {
                    got_complete = true;
                }
            }
        }
        assert!(!got_complete, "noise must never yield FileComplete");
    }

    #[test]
    fn progress_events_emitted_during_streaming() {
        // TX large enough that at least one progress tick fires during
        // Streaming (tick cadence = PROGRESS_TICK_EVERY_SEC). 2000 B on
        // NORMAL is ~7.5 s of audio — comfortably past one tick.
        let config = profile_normal();
        let data: Vec<u8> = (0..2000).map(|i| (i * 7 + 3) as u8).collect();
        let tx = make_v2_tx(&data, &config, 0xDEAD_BEEF);

        let mut stream = Vec::new();
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 0.5) as usize]);
        stream.extend_from_slice(&tx);
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 1.2) as usize]);

        let mut rx = StreamReceiverV2::new();
        let events = feed_all(&mut rx, &stream);

        let progress_events: Vec<(usize, usize, f64)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEventV2::ProgressUpdate {
                    blocks_converged,
                    blocks_total,
                    sigma2,
                    ..
                } => Some((*blocks_converged, *blocks_total, *sigma2)),
                _ => None,
            })
            .collect();
        assert!(
            !progress_events.is_empty(),
            "expected at least one ProgressUpdate during streaming, got {} events total",
            events.len()
        );
        for (bc, bt, _) in &progress_events {
            assert!(bc <= bt, "converged must not exceed total : {bc}/{bt}");
        }

        // AppHeaderDecoded must appear somewhere before FileComplete (the
        // tick path surfaces the meta segment as soon as it decodes).
        let first_app = events
            .iter()
            .position(|e| matches!(e, StreamEventV2::AppHeaderDecoded { .. }));
        let first_complete = events
            .iter()
            .position(|e| matches!(e, StreamEventV2::FileComplete { .. }));
        assert!(first_app.is_some(), "missing AppHeaderDecoded");
        if let (Some(a), Some(c)) = (first_app, first_complete) {
            assert!(a < c, "AppHeaderDecoded must precede FileComplete");
        }

        // Final payload still matches and session ended cleanly.
        let got_content = events.iter().find_map(|e| match e {
            StreamEventV2::FileComplete { content, .. } => Some(content.clone()),
            _ => None,
        });
        assert_eq!(got_content.as_deref(), Some(&data[..]));
        let end_reason = events.iter().rev().find_map(|e| match e {
            StreamEventV2::SessionEnd { reason } => Some(*reason),
            _ => None,
        });
        assert_eq!(end_reason, Some(SessionEndReason::Completed));
    }

    #[test]
    fn back_to_back_two_transmissions() {
        let config = profile_normal();
        let d1: Vec<u8> = (0..300).map(|i| (i * 17) as u8).collect();
        let d2: Vec<u8> = (0..350).map(|i| ((i * 29) ^ 0x5A) as u8).collect();
        let tx1 = make_v2_tx(&d1, &config, 0x1111_1111);
        let tx2 = make_v2_tx(&d2, &config, 0x2222_2222);

        let mut stream = Vec::new();
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 0.5) as usize]);
        stream.extend_from_slice(&tx1);
        // Between the two TX: long enough for SessionEnd + Idle scan to
        // pick up the second preamble cleanly (> T_silence_trigger + some
        // idle-scan margin, well below T_abort).
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 2.5) as usize]);
        stream.extend_from_slice(&tx2);
        stream.extend(vec![0.0f32; (AUDIO_RATE as f64 * 1.2) as usize]);

        let mut rx = StreamReceiverV2::new();
        let events = feed_all(&mut rx, &stream);

        let completes: Vec<Vec<u8>> = events
            .iter()
            .filter_map(|e| match e {
                StreamEventV2::FileComplete { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        let n_end_completed = events
            .iter()
            .filter(|e| matches!(
                e,
                StreamEventV2::SessionEnd {
                    reason: SessionEndReason::Completed
                }
            ))
            .count();

        assert_eq!(completes.len(), 2, "expected two FileComplete events");
        assert_eq!(completes[0], d1, "first TX payload");
        assert_eq!(completes[1], d2, "second TX payload");
        assert_eq!(n_end_completed, 2, "expected two SessionEnd{{Completed}}");
        assert!(!rx.is_locked(), "should end in Idle");
    }
}
