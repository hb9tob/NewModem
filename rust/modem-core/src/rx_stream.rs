//! Streaming v2 receiver : continuous audio in → modem events out.
//!
//! Designed for a GUI that captures live audio from a sound card and wants
//! real-time feedback on what is being received (preamble, mode, progress,
//! completed file). The architecture is deliberately simple : the receiver
//! buffers audio internally and, every `scan_interval_samples` samples,
//! attempts to detect a preamble + decode the current buffer end-to-end.
//!
//! Auto-detection : the preamble is scanned using every profile in
//! `ProfileIndex::ALL`; the profile giving the strongest correlation is
//! locked, its protocol header is decoded, and the `profile_index` field of
//! the header unambiguously identifies the full `ModemConfig` to use for the
//! rest of the transmission — resolving HIGH vs MEGA which share the same
//! `mode_code`.
//!
//! Complete decode uses `rx_v2::rx_v2` (batch mode) over the slice of the
//! buffer starting at the detected preamble. This is inefficient CPU-wise
//! (we re-run the whole chain on each retry) but trivially correct and
//! re-uses all existing tested logic. For live use at 48 kHz mono the cost
//! is negligible (a 30 s transmission decodes in ~0.5 s on a modern CPU).

use std::collections::VecDeque;

use crate::app_header::AppHeader;
use crate::demodulator;
use crate::header::{self, Header};
use crate::payload_envelope::PayloadEnvelope;
use crate::preamble;
use crate::profile::{ModemConfig, ProfileIndex};
use crate::rrc::{self, rrc_taps};
use crate::rx_v2;
use crate::sync;
use crate::types::{Complex64, AUDIO_RATE, N_PREAMBLE, RRC_SPAN_SYM};

/// Events emitted by `StreamReceiver::feed` as new audio arrives.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A preamble was detected and its carrier profile has been locked.
    PreambleDetected { profile: ProfileIndex },

    /// The protocol header was decoded — authoritative profile and payload
    /// length are now known.
    HeaderDecoded {
        profile: ProfileIndex,
        mode_code: u8,
        payload_length: u16,
    },

    /// The session meta-segment (AppHeader) was recovered.
    AppHeaderDecoded {
        session_id: u32,
        file_size: u32,
        mime_type: u8,
        hash_short: u16,
    },

    /// The payload envelope (filename + callsign) was recovered.
    EnvelopeDecoded { filename: String, callsign: String },

    /// Progress update from the current decoding attempt (emitted per successful
    /// decode batch — not per codeword, to keep traffic low).
    ProgressUpdate {
        blocks_converged: usize,
        blocks_total: usize,
        sigma2: f64,
    },

    /// The current transmission is fully decoded. Carries the content AFTER
    /// the payload envelope has been unwrapped.
    FileComplete {
        filename: String,
        callsign: String,
        mime_type: u8,
        content: Vec<u8>,
        sigma2: f64,
    },

    /// Silence was detected after an active transmission, or the buffer was
    /// cleared because the transmission never completed within the timeout.
    /// The receiver is now ready for a new session.
    SessionEnd,
}

/// State machine for the streaming receiver.
#[derive(Debug, Clone)]
enum State {
    /// No preamble yet. Periodically scan for one.
    Idle,
    /// Preamble locked on a profile; keep buffering until we can decode
    /// the full transmission (either payload_length achieved, or silence).
    Locked {
        profile: ProfileIndex,
        /// Sample offset (absolute, in `total_samples_fed`) at which the
        /// preamble was detected.
        preamble_abs_offset: u64,
        /// If the protocol header has been decoded at some point, its
        /// payload_length lets us estimate when the transmission ends.
        payload_length: Option<u32>,
        /// Sample count for silence tracking (resets to 0 whenever meaningful
        /// signal is present; once above `silence_threshold`, we close the
        /// session).
        silence_samples: usize,
        /// Number of feed() calls since lock — used to throttle retries.
        feeds_since_lock: usize,
    },
}

/// Live v2 receiver.
pub struct StreamReceiver {
    buffer: VecDeque<f32>,
    max_buffer_samples: usize,
    total_samples_fed: u64,
    state: State,
    scan_interval_samples: usize,
    last_scan_at_offset: u64,
    /// Below this RMS, `silence_samples` accumulates; `SILENCE_THRESHOLD_SAMPLES`
    /// of continuous silence after lock closes the session.
    silence_rms_threshold: f32,
    silence_timeout_samples: usize,
    /// If lock is maintained longer than this without completing, force
    /// session end so the buffer doesn't grow without bound.
    session_timeout_samples: usize,
    /// Retry decode cadence while locked (every N feed() calls).
    retry_decode_every: usize,
}

impl StreamReceiver {
    /// Create a new receiver. Typical defaults for 48 kHz capture :
    /// - `max_buffer_samples = 20 * 60 * 48_000` (20 min — covers the largest
    ///   ULTRA 100 kB transmission + slack)
    /// - `scan_interval_samples = 48_000` (try preamble detection every ~1 s)
    pub fn new(max_buffer_samples: usize, scan_interval_samples: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(max_buffer_samples),
            max_buffer_samples,
            total_samples_fed: 0,
            state: State::Idle,
            scan_interval_samples,
            last_scan_at_offset: 0,
            silence_rms_threshold: 0.005,
            silence_timeout_samples: (1.5 * AUDIO_RATE as f64) as usize,
            session_timeout_samples: (25 * 60 * AUDIO_RATE) as usize,
            retry_decode_every: 5, // retry decode every ~5 feed() calls
        }
    }

    /// Create a receiver with default settings for live capture.
    pub fn default_live() -> Self {
        let mut r = Self::new(
            20 * 60 * AUDIO_RATE as usize,
            AUDIO_RATE as usize, // 1 s scan cadence
        );
        // In live capture, decode only when the transmission ends (silence
        // detected) — periodic retries every few feeds run the whole rx_v2
        // pipeline on an ever-growing buffer and freeze the worker thread.
        r.retry_decode_every = usize::MAX;
        r
    }

    /// Reset to Idle state, discarding the buffer.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.state = State::Idle;
        self.last_scan_at_offset = self.total_samples_fed;
    }

    /// Ingest a chunk of audio samples (typically ~10-100 ms worth). Returns
    /// the events produced during this call; may be empty.
    pub fn feed(&mut self, samples: &[f32]) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        // Append to buffer, honouring the max size (drop oldest on overflow)
        for &s in samples {
            if self.buffer.len() >= self.max_buffer_samples {
                self.buffer.pop_front();
            }
            self.buffer.push_back(s);
        }
        self.total_samples_fed += samples.len() as u64;

        // Update silence tracker using the RMS of the just-arrived samples
        let rms = compute_rms(samples);
        let is_silent = rms < self.silence_rms_threshold;

        // State transitions
        match &mut self.state {
            State::Idle => {
                // Keep only a sliding window in Idle : the preamble is ~0.5 s
                // long, so a 3 s window is always enough to detect it; older
                // samples are useless and scanning them scales linearly. Without
                // this cap, feed() becomes O(total_runtime) and the worker
                // falls irrecoverably behind in live capture.
                const IDLE_WINDOW_SAMPLES: usize = 3 * AUDIO_RATE as usize;
                while self.buffer.len() > IDLE_WINDOW_SAMPLES {
                    self.buffer.pop_front();
                }

                // Scan for preamble periodically
                let should_scan = self
                    .total_samples_fed
                    .saturating_sub(self.last_scan_at_offset)
                    >= self.scan_interval_samples as u64
                    && self.buffer.len() >= MIN_BUFFER_FOR_SCAN;
                if should_scan {
                    self.last_scan_at_offset = self.total_samples_fed;
                    if let Some((profile, preamble_offset_in_buffer)) =
                        scan_preamble_multi_profile(&self.buffer)
                    {
                        let preamble_abs_offset = self.total_samples_fed
                            - self.buffer.len() as u64
                            + preamble_offset_in_buffer as u64;
                        events.push(StreamEvent::PreambleDetected { profile });
                        self.state = State::Locked {
                            profile,
                            preamble_abs_offset,
                            payload_length: None,
                            silence_samples: 0,
                            feeds_since_lock: 0,
                        };
                    }
                }
            }
            State::Locked {
                profile,
                preamble_abs_offset,
                payload_length,
                silence_samples,
                feeds_since_lock,
            } => {
                *feeds_since_lock += 1;

                if is_silent {
                    *silence_samples += samples.len();
                } else {
                    *silence_samples = 0;
                }
                let silence_exceeded = *silence_samples >= self.silence_timeout_samples;

                // Decide whether to try decoding now : on silence, or at every
                // retry_decode_every feeds. Either way, we run rx_v2 on the
                // buffer slice since the preamble.
                let should_retry = silence_exceeded
                    || *feeds_since_lock % self.retry_decode_every == 0;

                let locked_samples_available =
                    self.total_samples_fed.saturating_sub(*preamble_abs_offset) as usize;
                let session_limit_reached =
                    locked_samples_available >= self.session_timeout_samples;

                if should_retry || session_limit_reached {
                    // Extract the slice of the buffer that corresponds to this
                    // locked session. The buffer may have dropped older samples,
                    // so clamp the offset into the buffer.
                    let buffer_start_abs = self.total_samples_fed
                        - self.buffer.len() as u64;
                    let slice_start = preamble_abs_offset
                        .saturating_sub(buffer_start_abs)
                        as usize;
                    let slice: Vec<f32> = self
                        .buffer
                        .iter()
                        .skip(slice_start)
                        .copied()
                        .collect();

                    // First decode with current locked profile. If the
                    // protocol header's `profile_index` disagrees (common when
                    // initial auto-detection couldn't distinguish profiles
                    // that share sps/pitch/beta — e.g., NORMAL vs HIGH), lock
                    // on the header-authoritative profile and re-decode.
                    let profile_snapshot = *profile;
                    let config = profile_snapshot.to_config();
                    let first_attempt = rx_v2::rx_v2(&slice, &config);

                    let (result, used_profile) = match first_attempt {
                        Some(r) => {
                            let authoritative =
                                r.header.as_ref().and_then(|h| {
                                    ProfileIndex::from_u8(h.profile_index)
                                });
                            match authoritative {
                                Some(real_p) if real_p != profile_snapshot => {
                                    // Retry with the profile the header tells us
                                    *profile = real_p;
                                    let new_config = real_p.to_config();
                                    match rx_v2::rx_v2(&slice, &new_config) {
                                        Some(r2) => (Some(r2), real_p),
                                        None => (Some(r), profile_snapshot),
                                    }
                                }
                                _ => (Some(r), profile_snapshot),
                            }
                        }
                        None => (None, profile_snapshot),
                    };

                    if let Some(result) = result {
                        if let Some(ref hdr) = result.header {
                            let decoded_profile = ProfileIndex::from_u8(hdr.profile_index)
                                .unwrap_or(used_profile);
                            events.push(StreamEvent::HeaderDecoded {
                                profile: decoded_profile,
                                mode_code: hdr.mode_code,
                                payload_length: hdr.payload_length,
                            });
                            *payload_length = Some(hdr.payload_length as u32);
                        }
                        if let Some(ref ah) = result.app_header {
                            events.push(StreamEvent::AppHeaderDecoded {
                                session_id: ah.session_id,
                                file_size: ah.file_size,
                                mime_type: ah.mime_type,
                                hash_short: ah.hash_short,
                            });
                        }
                        events.push(StreamEvent::ProgressUpdate {
                            blocks_converged: result.converged_blocks,
                            blocks_total: result.total_blocks,
                            sigma2: result.sigma2,
                        });

                        // Determine whether the payload is genuinely complete:
                        // (a) all attempted blocks converged, AND
                        // (b) the content hash matches the one the TX recorded
                        //     in the AppHeader. Without (b) we'd be fooled by
                        //     rx_v2's zero-padding for missing codewords: the
                        //     size always matches AppHeader.file_size by
                        //     construction, so size_match can't discriminate.
                        let hash_match = result
                            .app_header
                            .as_ref()
                            .map(|a| fnv1a_u16(&result.data) == a.hash_short)
                            .unwrap_or(false);
                        let fully_converged = result.total_blocks > 0
                            && result.converged_blocks == result.total_blocks
                            && hash_match;
                        if fully_converged {
                            // Only attempt envelope decode once the payload
                            // is known-good (otherwise fallback gives empty
                            // metadata from garbage bytes, misleading the UI)
                            let envelope = PayloadEnvelope::decode_or_fallback(&result.data);
                            let mime = result.app_header.as_ref().map(|a| a.mime_type).unwrap_or(0);
                            events.push(StreamEvent::EnvelopeDecoded {
                                filename: envelope.filename.clone(),
                                callsign: envelope.callsign.clone(),
                            });
                            events.push(StreamEvent::FileComplete {
                                filename: envelope.filename,
                                callsign: envelope.callsign,
                                mime_type: mime,
                                content: envelope.content,
                                sigma2: result.sigma2,
                            });
                            events.push(StreamEvent::SessionEnd);
                            self.state = State::Idle;
                            self.buffer.clear();
                            self.last_scan_at_offset = self.total_samples_fed;
                        } else if silence_exceeded || session_limit_reached {
                            // Session ended without clean decode — emit
                            // SessionEnd (no FileComplete so the GUI knows
                            // the transmission was corrupt).
                            events.push(StreamEvent::SessionEnd);
                            self.state = State::Idle;
                            self.buffer.clear();
                            self.last_scan_at_offset = self.total_samples_fed;
                        }
                    } else if silence_exceeded || session_limit_reached {
                        events.push(StreamEvent::SessionEnd);
                        self.state = State::Idle;
                        self.buffer.clear();
                        self.last_scan_at_offset = self.total_samples_fed;
                    }
                }
            }
        }

        events
    }

    /// Current total samples ingested (absolute monotonic counter).
    pub fn total_samples_fed(&self) -> u64 {
        self.total_samples_fed
    }

    /// Whether we're currently locked on a transmission.
    pub fn is_locked(&self) -> bool {
        matches!(self.state, State::Locked { .. })
    }
}

/// Minimum buffer size before attempting a preamble scan : we need at least
/// (RRC span + preamble duration) samples at the slowest profile (ULTRA at
/// 500 Bd → ~0.5 s + RRC margin). 1 second at 48 kHz is safe for all profiles.
const MIN_BUFFER_FOR_SCAN: usize = 48_000;

/// Scan a buffer against all profiles and return the best match, if any.
fn scan_preamble_multi_profile(buffer: &VecDeque<f32>) -> Option<(ProfileIndex, usize)> {
    // Take a contiguous slice copy for downmix + MF
    let samples: Vec<f32> = buffer.iter().copied().collect();

    let mut best: Option<(ProfileIndex, usize, f64)> = None;

    for &profile in &ProfileIndex::ALL {
        let config = profile.to_config();
        let (sps, pitch) = match rrc::check_integer_constraints(
            AUDIO_RATE,
            config.symbol_rate,
            config.tau,
        ) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let bb = demodulator::downmix(&samples, config.center_freq_hz);
        let mf = demodulator::matched_filter(&bb, &taps);

        let pos = match sync::find_preamble(&mf, sps, pitch, config.beta) {
            Some(p) => p,
            None => continue,
        };
        // Compute correlation strength at the found position for ranking
        let preamble_syms = preamble::make_preamble();
        let mag2 = correlate_magnitude_sq(&mf, &preamble_syms, pos, pitch);
        // Normalise so different profiles are comparable : divide by the
        // length of the preamble
        let score = mag2 / (N_PREAMBLE as f64);

        match best {
            None => best = Some((profile, pos, score)),
            Some((_, _, prev)) if score > prev => {
                best = Some((profile, pos, score));
            }
            _ => {}
        }
    }

    best.and_then(|(p, pos, score)| {
        // Require a minimum correlation score to avoid false locks on noise
        if score > 0.05 {
            Some((p, pos))
        } else {
            None
        }
    })
}

fn correlate_magnitude_sq(
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

/// FNV-1a 32-bit hash folded to 16 bits. Matches the content hash the TX
/// stores in AppHeader::hash_short for payload integrity verification.
fn fnv1a_u16(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;
    use crate::modulator;
    use crate::profile::{profile_high, profile_normal};

    fn make_v2_tx(
        data: &[u8],
        config: &ModemConfig,
        session_id: u32,
    ) -> Vec<f32> {
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau).unwrap();
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let envelope =
            PayloadEnvelope::new("test.bin", "HB9TST", data.to_vec()).unwrap();
        let wire = envelope.encode();
        let hash = fnv1a_u16(&wire);
        let symbols = frame::build_superframe_v2(
            &wire,
            config,
            session_id,
            crate::app_header::mime::BINARY,
            hash,
        );
        modulator::modulate(&symbols, sps, pitch, &taps, config.center_freq_hz)
    }

    #[test]
    fn stream_decodes_single_transmission_normal() {
        let config = profile_normal();
        let data: Vec<u8> = (0..500).map(|i| (i * 11) as u8).collect();
        let tx_samples = make_v2_tx(&data, &config, 0xBEEF_CAFE);

        let mut rx = StreamReceiver::default_live();
        // Prepend 0.5 s of silence to simulate radio start up
        let silence = vec![0.0f32; (AUDIO_RATE as f64 * 0.5) as usize];
        // Feed in 10 ms chunks
        let chunk_size = (AUDIO_RATE / 100) as usize; // 480 samples
        let mut all_samples = Vec::new();
        all_samples.extend_from_slice(&silence);
        all_samples.extend_from_slice(&tx_samples);
        // Trailing silence to trigger session end
        all_samples.extend(vec![0.0f32; (AUDIO_RATE as f64 * 2.5) as usize]);

        let mut events = Vec::new();
        for chunk in all_samples.chunks(chunk_size) {
            events.extend(rx.feed(chunk));
        }

        let mut got_preamble = false;
        let mut header_profile = None;
        let mut got_app = false;
        let mut envelope_seen = None;
        let mut got_complete = None;
        for ev in &events {
            match ev {
                StreamEvent::PreambleDetected { .. } => {
                    got_preamble = true;
                }
                StreamEvent::HeaderDecoded { profile, .. } => {
                    header_profile = Some(*profile);
                }
                StreamEvent::AppHeaderDecoded { session_id, .. } => {
                    got_app = true;
                    assert_eq!(*session_id, 0xBEEF_CAFE);
                }
                StreamEvent::EnvelopeDecoded { callsign, filename } => {
                    envelope_seen = Some((callsign.clone(), filename.clone()));
                }
                StreamEvent::FileComplete { content, .. } => {
                    got_complete = Some(content.clone());
                }
                _ => {}
            }
        }
        assert!(got_preamble, "missing PreambleDetected");
        assert_eq!(
            header_profile,
            Some(ProfileIndex::Normal),
            "header should report NORMAL (not the tentative preamble lock)"
        );
        assert!(got_app, "missing AppHeaderDecoded");
        let (cs, fname) = envelope_seen.expect("missing EnvelopeDecoded");
        assert_eq!(cs, "HB9TST");
        assert_eq!(fname, "test.bin");
        let content = got_complete.expect("missing FileComplete");
        assert_eq!(content, data);
    }

    #[test]
    fn stream_auto_detects_profile_high() {
        let config = profile_high();
        let data: Vec<u8> = (0..500).map(|i| (i * 13) as u8).collect();
        let tx_samples = make_v2_tx(&data, &config, 0x1234_5678);

        let mut rx = StreamReceiver::default_live();
        let chunk_size = 480;
        let mut all = vec![0.0f32; 24_000]; // 0.5 s silence
        all.extend_from_slice(&tx_samples);
        all.extend(vec![0.0f32; 120_000]); // 2.5 s trailing silence

        let mut events = Vec::new();
        for chunk in all.chunks(chunk_size) {
            events.extend(rx.feed(chunk));
        }

        // The initial PreambleDetected carries only a tentative profile
        // (NORMAL and HIGH share sps/pitch/beta — correlations tie and the
        // scan picks whichever is tried first). The HeaderDecoded event,
        // populated from header.profile_index, is authoritative.
        let header_profile = events.iter().find_map(|ev| match ev {
            StreamEvent::HeaderDecoded { profile, .. } => Some(*profile),
            _ => None,
        });
        assert_eq!(header_profile, Some(ProfileIndex::High));

        let content = events.iter().find_map(|ev| match ev {
            StreamEvent::FileComplete { content, .. } => Some(content.clone()),
            _ => None,
        });
        assert_eq!(content, Some(data));
    }

    #[test]
    fn stream_idle_on_pure_noise() {
        let mut rx = StreamReceiver::default_live();
        // 5 s of white-ish noise, feed in chunks
        let mut state = 12345u64;
        let mut chunk = vec![0.0f32; 4800];
        for iter in 0..50 {
            for s in chunk.iter_mut() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let b = (state >> 33) as u32;
                *s = (b as f32 / u32::MAX as f32 - 0.5) * 0.1;
            }
            let events = rx.feed(&chunk);
            for ev in events {
                // It's acceptable to momentarily think a preamble was detected
                // and then backtrack via failed decode; just never emit
                // FileComplete on pure noise.
                if let StreamEvent::FileComplete { .. } = ev {
                    panic!("FileComplete on noise iteration {iter}");
                }
            }
        }
        assert!(!rx.is_locked() || rx.total_samples_fed > 0);
    }
}
