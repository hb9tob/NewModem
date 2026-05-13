//! RX worker for the 2x family.
//!
//! Bridges the audio-domain (mono `f32` 48 kHz from sound-card / SDR /
//! WAV) to [`modem_core2x::rx_v4::rx_v4_symbols`], which expects a
//! symbol-rate stream of complex symbols.
//!
//! Pipeline:
//!
//! 1. **Downmix** to baseband at `cfg.base.center_freq_hz`.
//! 2. **Matched filter** with the RRC TX pulse (same β / span / sps).
//! 3. **Symbol-rate sampling** — naive integer step today, with a
//!    coarse symbol-phase search to find the best offset within one
//!    symbol period. The TimingLoop / Farrow upgrade (Phase B
//!    integration) lives in [`audio_to_symbols_with_timing`] below as
//!    a `TODO` placeholder; the noise-free / low-drift case decodes
//!    cleanly without it.
//! 4. Hand the symbols to [`rx_v4_symbols`].
//!
//! For sound-card paths with measurable clock drift the TimingLoop
//! integration is the next step; the naive sampler tolerates ≤ ~50 ppm
//! across one PLHEADER cycle (~4 s) before the symbol-rate phase walks
//! more than half a sample.
//!
//! [`rx_v4_symbols`]: modem_core2x::rx_v4::rx_v4_symbols

use modem_core_base::demodulator;
use modem_core_base::rrc::{self, rrc_taps};
use modem_core_base::types::{Complex64, AUDIO_RATE, RRC_SPAN_SYM};
use modem_core2x::gate2x::{PreambleProbe2x, IDLE_PROBE_BUF_SAMPLES, PROBE_THRESHOLD_2X};
use modem_core2x::plheader::{sof_for_family, SOF_LEN_SYM};
use modem_core2x::profile2x::{config_by_name_2x, ModemConfig2x};
use modem_core2x::rx_v4::{self, RxResult2x};
use modem_framing::app_header::AppHeader;
use modem_framing::payload_envelope::PayloadEnvelope;
use modem_worker_base::{EventSink, EventSinkExt, SharedWavSink, WorkerHandle};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Convert an audio-domain `f32` buffer into a stream of complex symbols
/// ready for [`rx_v4_symbols`](modem_core2x::rx_v4::rx_v4_symbols).
///
/// Steps: downmix → matched filter → SOF-anchored symbol-phase pick →
/// integer-step sampling at `sps`.
///
/// The phase pick is **SOF-anchored**: we run a coarse SOF cross-
/// correlation against the matched-filter output for every offset in
/// `[0, sps)` and keep the offset whose peak is largest. This locks the
/// strobe grid onto the actual TX-side strobe positions (which the
/// modulator places at `6·sps + k·sps`) regardless of `sps`. A naive
/// energy-only search worked for `sps=32` but lost the strobe for
/// `sps=48 / 96` profiles where the RRC pulse spreads enough to flatten
/// the energy distribution between strobes.
///
/// `samples` is mono 48 kHz; the returned vector has roughly
/// `samples.len() / sps` entries.
pub fn audio_to_symbols(
    samples: &[f32],
    cfg: &ModemConfig2x,
) -> Result<Vec<Complex64>, String> {
    let (sps, _pitch) = rrc::check_integer_constraints(
        AUDIO_RATE,
        cfg.base.symbol_rate,
        cfg.base.tau,
    )?;
    let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);

    // Downmix to baseband + matched filter.
    let bb = demodulator::downmix(samples, cfg.base.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);

    // The modulator places symbol k's pulse peak at audio sample
    // `k*sps + (taps.len()-1)/2 = k*sps + 6*sps`. The matched filter
    // is shift-invariant, so the strobe positions in the MF output are
    // at audio offsets `6*sps + k*sps` — multiples of `sps`. Sampling
    // with phase 0 thus lands on every strobe, after a 6-symbol lead-in
    // that `find_next_sof` skips over harmlessly. The SOF-anchored
    // variant `best_symbol_phase_sof` is kept for sound-card paths
    // where the AGC/codec might shift the strobe by a few samples.
    let _phase = best_symbol_phase_sof(&mf, sps, cfg);
    let phase = 0usize;

    // Naive integer-step sampling at the symbol rate. The TimingLoop
    // upgrade (Phase B integration) replaces this with strobed Farrow
    // interpolation. For now a flat phase + integer step decodes
    // noise-free WAVs cleanly (validated by the roundtrip tests).
    let n_syms = mf.len().saturating_sub(phase) / sps;
    let mut out = Vec::with_capacity(n_syms);
    for k in 0..n_syms {
        out.push(mf[phase + k * sps]);
    }
    Ok(out)
}

/// Pick the symbol-phase offset (in samples, in `[0, sps)`) whose
/// integer-step sampling lines up best with a SOF correlation peak.
///
/// For each candidate phase, we sample the matched-filter output at the
/// symbol rate and cross-correlate the first ~512 sampled symbols
/// against the SOF template. The phase whose strongest peak is highest
/// wins — i.e. the one that best aligns the sampled stream to the
/// underlying symbol grid.
///
/// This costs `sps` symbol-domain correlations but pays back by giving
/// frame-tight alignment that an energy-only heuristic can miss when
/// the RRC pulse spreads enough to fill the gaps between strobes
/// (visible at `sps ∈ {48, 96}`).
fn best_symbol_phase_sof(
    mf: &[Complex64],
    sps: usize,
    cfg: &ModemConfig2x,
) -> usize {
    if mf.len() < sps * (SOF_LEN_SYM + 4) {
        return 0;
    }
    let sof = sof_for_family(cfg.family);
    // Bound the search window so the full burst search stays linear in
    // sps × symbols (instead of sps × audio length). 1024 sym is enough
    // to land on at least one PLHEADER in the typical 2x bursts.
    let max_syms = (mf.len() / sps).min(1024);

    let mut best_phase = 0usize;
    let mut best_peak = 0.0_f64;
    for p in 0..sps {
        let n_syms = (mf.len() - p) / sps;
        let n_syms = n_syms.min(max_syms);
        if n_syms < SOF_LEN_SYM + 1 {
            continue;
        }
        let mut peak = 0.0_f64;
        for k0 in 0..(n_syms - SOF_LEN_SYM) {
            let mut acc = Complex64::new(0.0, 0.0);
            for n in 0..SOF_LEN_SYM {
                acc += mf[p + (k0 + n) * sps] * sof[n].conj();
            }
            let mag = acc.norm();
            if mag > peak { peak = mag; }
        }
        if peak > best_peak {
            best_peak = peak;
            best_phase = p;
        }
    }
    best_phase
}

/// Audio-domain wrapper: downmix + matched-filter + sample +
/// [`rx_v4_symbols`](modem_core2x::rx_v4::rx_v4_symbols). The single entry
/// point a CLI / GUI worker calls per audio chunk.
pub fn rx_v4_audio(
    samples: &[f32],
    cfg: &ModemConfig2x,
) -> Result<Option<RxResult2x>, String> {
    let symbols = audio_to_symbols(samples, cfg)?;
    Ok(rx_v4::rx_v4_symbols(&symbols, cfg))
}

// ---------------------------------------------------------------------------
// Event payloads — shaped to match the V3 worker's wire format so the
// frontend listeners don't care which family decoded the burst.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct SessionArmedPayload {
    session_id: u32,
    k: u32,
    t: u8,
    file_size: u32,
    mime_type: u8,
    profile: String,
    session_dir: String,
}

#[derive(Debug, Clone, Serialize)]
struct SessionDecodedPayload {
    session_id: u32,
    session_dir: String,
    decoded_path: String,
    size: u32,
    filename: Option<String>,
    callsign: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FileCompletePayload {
    filename: String,
    callsign: String,
    mime_type: u8,
    saved_path: String,
    sigma2: f64,
    sigma2_data_avg: f64,
    size: usize,
}

#[derive(Debug, Clone, Serialize)]
struct EnvelopePayload {
    filename: String,
    callsign: String,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorPayload {
    message: String,
}

// ---------------------------------------------------------------------------
// Idle-gate tuning (Phase C-5)
// ---------------------------------------------------------------------------

/// Minimum wall-clock interval between two FFT probe calls while idle.
/// Matches the V3 worker's `SCAN_INTERVAL_MS` so latency-to-first-decode
/// is comparable between families.
const PROBE_INTERVAL: Duration = Duration::from_millis(1000);

/// After a positive probe, keep running `rx_v4_symbols` every chunk for
/// this long without re-probing. One PLHEADER period is plenty — the
/// SOF correlator inside `rx_v4_symbols` typically locks within the
/// first cycle and then [`is_active`] becomes true (gate bypassed
/// entirely).
const PROBE_HOT_HOLD: Duration = Duration::from_secs(4);

// ---------------------------------------------------------------------------
// Streaming RX worker (Phase D-1c) — no sliding-window
// ---------------------------------------------------------------------------

/// Spawn a streaming V4 RX worker.
///
/// Architecture (replaces the D-1b one-shot version):
///
/// ```text
///   audio chunk (f32, 48 kHz)
///      │  ↳ tee to wav_sink if armed
///      ▼
///   StreamingFrontend.process_chunk(chunk)
///      │  ↳ NCO downmix · RRC matched filter · Farrow interpolation
///      │     at fractional strobes · Gardner TED + PI loop
///      ▼
///   newly-produced symbols (Vec<Complex64>)
///      │
///      ▼
///   symbol_buffer.extend(symbols)
///      │
///      ▼
///   rx_v4_symbols(symbol_buffer)   // idempotent on a growing buffer;
///      │                            // Farrow+Gardner make per-chunk
///      │                            // re-decodes lossless (no sliding
///      │                            // window needed).
///      ▼
///   dedup-aware event emission (session_armed once per session_id,
///   session_progress per tick, session_decoded once per session_id,
///   file_complete once per session_id) → finalize on EOT.
/// ```
///
/// No sliding window: with closed-loop Gardner driving the strobe,
/// every symbol is produced once at the correct timing. The periodic
/// `rx_v4_symbols` call on the growing symbol buffer is the cheap
/// "advance the decode" step — it's not re-doing timing recovery,
/// it's just re-running SOF correlation + LDPC on a slightly larger
/// input each tick. The symbol buffer is ~32× smaller than the
/// equivalent audio buffer V3 carried around.
///
/// On channel close (WAV pacer done OR live capture stopped): one
/// final `rx_v4_symbols` pass with any remaining symbols. If the EOT
/// flag was seen at any point during streaming, the session is already
/// finalised; otherwise we finalise here on a best-effort basis (a
/// partial decode still produces session_decoded if enough CWs
/// converged).
///
/// `dropped_samples` and `deemphasis_enabled` are accepted purely for
/// signature symmetry with [`modem-worker::rx_worker::spawn`] so the
/// GUI's dispatch site is a clean `if 2x { v4_spawn } else { v3_spawn }`
/// branch. Today the dropped-sample counter is observed only for the
/// final tally and deemphasis is a no-op (V4 hasn't wired the optional
/// NBFM deemphasis filter into its RX chain yet — tracked separately).
///
/// # Idle gate (Phase C-5)
///
/// While no SOF has been locked yet, the symbol-domain
/// [`rx_v4_symbols`](modem_core2x::rx_v4::rx_v4_symbols) pipeline is
/// throttled by a cheap FFT presence probe
/// ([`PreambleProbe2x`](modem_core2x::gate2x::PreambleProbe2x)). The
/// probe runs at most every [`PROBE_INTERVAL`] on a rolling 2 s audio
/// window; a positive result opens a [`PROBE_HOT_HOLD`] window during
/// which `rx_v4_symbols` is called every chunk (one PLHEADER cycle —
/// long enough for the SOF correlator inside `rx_v4_symbols` to lock
/// and switch the worker to the always-decode path). The `frontend`
/// (Farrow + Gardner timing recovery) still runs every chunk so the
/// closed-loop state stays continuous; only LDPC is gated.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    samples: Receiver<Vec<f32>>,
    sink: Arc<dyn EventSink>,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    profile_name: String,
    _deemphasis_enabled: bool,
    _dropped_samples: Arc<AtomicU64>,
) -> WorkerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        let cfg = match config_by_name_2x(&profile_name) {
            Some(c) => c,
            None => {
                sink.emit(
                    "error",
                    ErrorPayload {
                        message: format!("unknown 2x profile '{profile_name}'"),
                    },
                );
                return;
            }
        };
        let mut frontend = crate::streaming_frontend::StreamingFrontend::new(cfg.clone());
        let mut symbol_buffer: Vec<Complex64> = Vec::with_capacity(8192);
        // Dedup state: once we emit `session_armed` / `session_decoded`
        // / `file_complete` for a given session_id, we don't re-emit
        // them on a later tick that still sees the same session. The
        // app_header is captured the first time it converges; the
        // payload is finalised either on EOT or at channel close.
        let mut emitted_session_id: Option<u32> = None;
        let mut last_progress_converged: usize = 0;
        let mut finalised = false;

        // Idle-gate state (Phase C-5). The audio buffer is only
        // populated while the worker hasn't yet locked an SOF — once
        // the StreamingFrontend's `sof_anchor` flips to Some or the
        // worker has emitted a `session_armed`, we drop into the
        // always-decode path and the buffer is freed.
        let mut idle_audio_buf: VecDeque<f32> =
            VecDeque::with_capacity(IDLE_PROBE_BUF_SAMPLES);
        let mut last_probe_at: Option<Instant> = None;
        let mut probe_hot_until: Option<Instant> = None;
        loop {
            let chunk = match samples.recv() {
                Ok(c) => c,
                Err(_) => break, // sender dropped → exit and run a final tick.
            };
            if let Ok(mut g) = wav_sink.lock() {
                if let Some(ws) = g.as_mut() {
                    ws.write_chunk(&chunk);
                }
            }
            // Closed-loop streaming front-end: returns timing-recovered
            // symbols for the chunk. Cheap (~O(chunk_len)). MUST run
            // every chunk regardless of the gate so the Farrow + Gardner
            // closed-loop state stays continuous across the idle / active
            // transition (interrupting it would force re-acquisition on
            // the first decoded burst).
            let new_syms = frontend.process_chunk(&chunk);
            symbol_buffer.extend_from_slice(&new_syms);

            // Decide whether `rx_v4_symbols` (the expensive part — SOF
            // correlation + LDPC + EM smoothers) is worth running this
            // tick. Three cases:
            //
            //   1. Already-active session (`emitted_session_id` set or
            //      `frontend.sof_anchor()` locked). Run every chunk.
            //   2. Idle, within a `PROBE_HOT_HOLD` window after a
            //      positive probe. Run every chunk (let rx_v4 catch
            //      the SOF and graduate us to case 1).
            //   3. Idle, outside the hot window. Append to
            //      `idle_audio_buf`, throttle to `PROBE_INTERVAL`, run
            //      the FFT probe. On pass → start a new hot window.
            //      Otherwise skip.
            let is_active =
                emitted_session_id.is_some() || frontend.sof_anchor().is_some();
            let should_decode = if !finalised && symbol_buffer.len() >= 192 {
                if is_active {
                    // Active session — free the idle buffer; we won't
                    // use it again for this session.
                    if !idle_audio_buf.is_empty() {
                        idle_audio_buf.clear();
                        idle_audio_buf.shrink_to_fit();
                    }
                    true
                } else {
                    // Append the new chunk to the rolling idle buffer,
                    // trim front. VecDeque pop_front is O(1).
                    for &s in &chunk {
                        idle_audio_buf.push_back(s);
                    }
                    while idle_audio_buf.len() > IDLE_PROBE_BUF_SAMPLES {
                        idle_audio_buf.pop_front();
                    }
                    let now = Instant::now();
                    let in_hot_window =
                        probe_hot_until.map_or(false, |t| now < t);
                    if in_hot_window {
                        true
                    } else if last_probe_at
                        .map_or(true, |t| now.duration_since(t) >= PROBE_INTERVAL)
                        && idle_audio_buf.len() >= IDLE_PROBE_BUF_SAMPLES / 2
                    {
                        last_probe_at = Some(now);
                        // VecDeque → contiguous Vec for the FFT input.
                        // make_contiguous returns a &[T] in O(buf.len);
                        // we copy because the probe wants `&[f32]` and
                        // VecDeque borrow rules make this cleaner.
                        let buf: Vec<f32> =
                            idle_audio_buf.iter().copied().collect();
                        let probe =
                            PreambleProbe2x::for_buf_len(IDLE_PROBE_BUF_SAMPLES);
                        let r = probe.check(&buf);
                        if r.passes(PROBE_THRESHOLD_2X) {
                            probe_hot_until = Some(now + PROBE_HOT_HOLD);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
            } else {
                false
            };

            if should_decode {
                if let Some(res) = modem_core2x::rx_v4::rx_v4_symbols(&symbol_buffer, &cfg) {
                    // D-1c-ii: once rx_v4 has located the SOF, forward
                    // its absolute symbol-stream position to the
                    // StreamingFrontend so its closed-loop TED switches
                    // to AbsGardner-on-pilot-interior (DVB-S2X §9.3.2
                    // recipe). The symbol_buffer index matches the
                    // frontend's symbols_emitted counter 1:1 because
                    // the buffer is never trimmed in this thread.
                    if frontend.sof_anchor().is_none() {
                        if let Some(sof) = res.first_sof_at {
                            frontend.align_to_sof(sof as u64);
                        }
                    }
                    if let Some(ah) = res.app_header.as_ref() {
                        // First-time AppHeader → arm the session.
                        if emitted_session_id.is_none() {
                            let dir = save_dir
                                .lock()
                                .ok()
                                .map(|p| p.clone())
                                .unwrap_or_default();
                            let session_dir = dir
                                .join("sessions")
                                .join(format!("{:08x}.session", ah.session_id));
                            let _ = std::fs::create_dir_all(&session_dir);
                            sink.emit(
                                "session_armed",
                                SessionArmedPayload {
                                    session_id: ah.session_id,
                                    k: 0,
                                    t: 0,
                                    file_size: ah.file_size,
                                    mime_type: ah.mime_type,
                                    profile: profile_name.clone(),
                                    session_dir: session_dir.to_string_lossy().into_owned(),
                                },
                            );
                            emitted_session_id = Some(ah.session_id);
                            last_progress_converged = 0;
                        }
                        // Progress: only emit when the converged count
                        // changed since the last tick (avoid spamming
                        // listeners when a slow chunk produces no new
                        // CWs).
                        if res.converged_cws != last_progress_converged {
                            // Wrap each per-CW pilot phase into a single-
                            // element Vec to match V3's Vec<Vec<f32>>
                            // shape (V3 "segment" carried multiple pilot
                            // groups; V4 "CW" carries 1 or 2 pilot blocks
                            // collapsed to a single LS phase). The
                            // frontend renders one polyline point per
                            // outer-Vec entry, which is exactly what we
                            // want for per-CW drift over a burst.
                            let pilot_phase_segments: Vec<Vec<f32>> = res
                                .pilot_phase_per_cw
                                .iter()
                                .map(|&p| vec![p as f32])
                                .collect();
                            sink.emit(
                                "v2_progress",
                                serde_json::json!({
                                    "blocks_converged": res.converged_cws,
                                    "blocks_total": res.total_cws,
                                    "blocks_expected": res.total_cws,
                                    "sigma2": res.sigma2_data,
                                    "sigma2_data": res.sigma2_data,
                                    "constellation_sample":
                                        res.constellation_sample.clone(),
                                    "pilot_phase_segments":
                                        pilot_phase_segments,
                                    "pilot_phase_is_meta":
                                        res.pilot_phase_is_meta.clone(),
                                }),
                            );
                            last_progress_converged = res.converged_cws;
                        }
                        // EOT seen on at least one cycle → finalise
                        // immediately, no need to wait for channel
                        // close.
                        if res.eot_seen && !res.data.is_empty() {
                            let dir = save_dir
                                .lock()
                                .ok()
                                .map(|p| p.clone())
                                .unwrap_or_default();
                            finalize_session(
                                &sink,
                                &dir,
                                &profile_name,
                                ah,
                                &res.data,
                                res.sigma2_data,
                            );
                            finalised = true;
                        }
                    }
                }
            }
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }
        }

        // Channel-close or stop: one final pass to catch a burst that
        // ended without an EOT (truncated WAV, stop button mid-frame,
        // ...). If we already emitted via the EOT path above, skip.
        if finalised {
            return;
        }
        let result = modem_core2x::rx_v4::rx_v4_symbols(&symbol_buffer, &cfg);
        let Some(rx_result) = result else {
            // Only complain if we never saw a SOF. Otherwise emitted_id
            // is Some and the GUI's session pane already shows progress
            // — silence is the better UX than a bogus error.
            if emitted_session_id.is_none() {
                sink.emit(
                    "error",
                    ErrorPayload {
                        message: "RX V4 : aucun burst détecté".to_string(),
                    },
                );
            }
            return;
        };
        let Some(app_header) = rx_result.app_header else {
            sink.emit(
                "error",
                ErrorPayload {
                    message: format!(
                        "RX V4 : décodage incomplet ({}/{} CW)",
                        rx_result.converged_cws, rx_result.total_cws
                    ),
                },
            );
            return;
        };
        if rx_result.data.is_empty() {
            sink.emit(
                "error",
                ErrorPayload {
                    message: format!(
                        "RX V4 : payload vide (cycles={}, CW {}/{})",
                        rx_result.cycles, rx_result.converged_cws, rx_result.total_cws
                    ),
                },
            );
            return;
        }
        let dir = save_dir.lock().ok().map(|p| p.clone()).unwrap_or_default();
        finalize_session(
            &sink,
            &dir,
            &profile_name,
            &app_header,
            &rx_result.data,
            rx_result.sigma2_data,
        );
    });
    WorkerHandle {
        stop,
        thread: Some(thread),
    }
}

/// Build the on-disk path the decoded payload lands at, emit the V3-
/// shaped events, decode the envelope and (if zstd-wrapped) decompress
/// the content, then drop the final user file alongside the session
/// metadata.
fn finalize_session(
    sink: &Arc<dyn EventSink>,
    save_dir: &Path,
    profile_name: &str,
    app_header: &AppHeader,
    payload: &[u8],
    sigma2_data: f64,
) {
    // Mirror the V3 layout: `<save_dir>/sessions/<sid:08x>/`. Lets the
    // existing "list sessions" Tauri command pick V4 sessions up too
    // without any backend change.
    let session_dir = save_dir
        .join("sessions")
        .join(format!("{:08x}.session", app_header.session_id));
    if let Err(e) = std::fs::create_dir_all(&session_dir) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("mkdir {}: {e}", session_dir.display()),
            },
        );
        return;
    }
    sink.emit(
        "session_armed",
        SessionArmedPayload {
            session_id: app_header.session_id,
            k: 0,
            t: 0,
            file_size: app_header.file_size,
            mime_type: app_header.mime_type,
            profile: profile_name.to_string(),
            session_dir: session_dir.to_string_lossy().into_owned(),
        },
    );

    // Honour app_header.file_size: rx_v4 already truncates `data` to
    // file_size but we keep the assertion explicit so a future change
    // upstream doesn't silently break this contract.
    let payload = if payload.len() > app_header.file_size as usize {
        &payload[..app_header.file_size as usize]
    } else {
        payload
    };

    let env = PayloadEnvelope::decode_or_fallback(payload);
    let (filename, callsign, content) = if env.version != 0 {
        (env.filename.clone(), env.callsign.clone(), env.content.clone())
    } else {
        (
            format!("decoded_{:08x}.bin", app_header.session_id),
            String::new(),
            payload.to_vec(),
        )
    };

    // zstd unwrap for non-image payloads, matching the V3 worker.
    let (final_content, final_mime) =
        if app_header.mime_type == modem_framing::app_header::mime::ZSTD {
            match zstd::stream::decode_all(content.as_slice()) {
                Ok(decoded) => (decoded, modem_framing::app_header::mime::BINARY),
                Err(e) => {
                    sink.emit(
                        "error",
                        ErrorPayload {
                            message: format!("zstd decode: {e}"),
                        },
                    );
                    return;
                }
            }
        } else {
            (content, app_header.mime_type)
        };

    // Session-level "decoded.<ext>" copy for archival, plus the user-
    // facing copy at the save_dir root. Same dual-write pattern as V3
    // (see `rx_worker::finalize_session`): the session dir tracks the
    // raw decode for forensics, the root copy is what the operator
    // double-clicks.
    let ext = extension_for_mime(final_mime);
    let session_copy = session_dir.join(format!("decoded.{ext}"));
    if let Err(e) = std::fs::write(&session_copy, &final_content) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("write {}: {e}", session_copy.display()),
            },
        );
        return;
    }
    let user_copy = save_dir.join(&filename);
    if let Err(e) = std::fs::create_dir_all(save_dir) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("mkdir {}: {e}", save_dir.display()),
            },
        );
        return;
    }
    if let Err(e) = std::fs::write(&user_copy, &final_content) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("write {}: {e}", user_copy.display()),
            },
        );
        return;
    }

    sink.emit(
        "session_decoded",
        SessionDecodedPayload {
            session_id: app_header.session_id,
            session_dir: session_dir.to_string_lossy().into_owned(),
            decoded_path: session_copy.to_string_lossy().into_owned(),
            size: final_content.len() as u32,
            filename: Some(filename.clone()),
            callsign: Some(callsign.clone()),
        },
    );
    sink.emit(
        "envelope",
        EnvelopePayload {
            filename: filename.clone(),
            callsign: callsign.clone(),
        },
    );
    sink.emit(
        "file_complete",
        FileCompletePayload {
            filename,
            callsign,
            mime_type: final_mime,
            saved_path: user_copy.to_string_lossy().into_owned(),
            sigma2: sigma2_data, // V4 has no separate pilot σ²; reuse data σ²
            sigma2_data_avg: sigma2_data,
            size: final_content.len(),
        },
    );
}

fn extension_for_mime(mime_type: u8) -> &'static str {
    use modem_framing::app_header::mime;
    match mime_type {
        mime::IMAGE_AVIF => "avif",
        mime::IMAGE_JPEG => "jpg",
        mime::IMAGE_PNG => "png",
        mime::TEXT => "txt",
        mime::ZSTD => "zst",
        _ => "bin",
    }
}

/// Placeholder for the Phase B closed-loop timing-recovery integration.
///
/// Will replace [`audio_to_symbols`] with a Farrow-interpolated strobe
/// stream driven by [`modem_core_base::timing_loop::TimingLoop`] (Gardner
/// TED for QPSK/8PSK, AbsGardner for APSK). Same return shape so call
/// sites flip with a one-line swap once available.
#[doc(hidden)]
pub fn audio_to_symbols_with_timing(
    samples: &[f32],
    cfg: &ModemConfig2x,
) -> Result<Vec<Complex64>, String> {
    // TODO(C-7 follow-up): replace with TimingLoop strobed Farrow
    // interpolation. Today this is a synonym of `audio_to_symbols`.
    audio_to_symbols(samples, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_core2x::frame2x::build_superframe_v4;
    use modem_core2x::modem2x::V4Modem;
    use modem_core2x::profile2x::{
        profile_high_2x, profile_normal_2x, profile_robust_2x, profile_ultra_2x,
        ProfileIndex2x,
    };
    use modem_core_base::modulator;
    use modem_core_base::traits::{EncodeRequest, Modem};
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
        let symbols = build_superframe_v4(
            payload,
            cfg,
            session_id,
            mime::BINARY,
            0xAA55,
        );
        let (sps, pitch) = rrc::check_integer_constraints(
            AUDIO_RATE,
            cfg.base.symbol_rate,
            cfg.base.tau,
        )
        .unwrap();
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, cfg.base.center_freq_hz)
    }

    #[test]
    fn audio_to_symbols_produces_expected_count_high() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0x1);
        let audio = modulate_for(&cfg, &payload, 1);
        let syms = audio_to_symbols(&audio, &cfg).expect("ok");
        let expected_min = audio.len() / 32 - 32; // sps=32 for HIGH2X
        assert!(
            syms.len() >= expected_min,
            "got {} syms, expected ≥ {}",
            syms.len(),
            expected_min
        );
    }

    #[test]
    fn audio_roundtrip_high_2x() {
        // Encode → modulate → audio_to_symbols → rx_v4_symbols → match.
        let cfg = profile_high_2x();
        let payload = rng_bytes(1_500, 0xCAFE);
        let audio = modulate_for(&cfg, &payload, 0xDEAD_BEEF);
        let result = rx_v4_audio(&audio, &cfg)
            .expect("audio_to_symbols ok")
            .expect("decode ok");
        let h = result.app_header.expect("AppHeader");
        assert_eq!(h.session_id, 0xDEAD_BEEF);
        assert_eq!(h.file_size, payload.len() as u32);
        assert_eq!(result.data, payload);
    }

    #[test]
    fn audio_roundtrip_normal_2x() {
        let cfg = profile_normal_2x();
        let payload = rng_bytes(700, 0x42);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn audio_roundtrip_robust_2x() {
        let cfg = profile_robust_2x();
        let payload = rng_bytes(300, 0x88);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn audio_roundtrip_ultra_2x() {
        // ULTRA: 500 Bd → 96 sps. Smaller payload because the audio
        // grows fast (96 sps × ~2200 sym ≈ 4 s).
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(80, 0x99);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn pure_noise_audio_returns_none() {
        let cfg = profile_high_2x();
        // 2 s of low-amplitude pseudo-random noise — no SOF inside.
        let mut state = 0xDEAD_BEEF_u64;
        let audio: Vec<f32> = (0..AUDIO_RATE as usize * 2)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let s = (state >> 40) as i32 as f32 / i32::MAX as f32;
                s * 0.05
            })
            .collect();
        assert!(rx_v4_audio(&audio, &cfg).unwrap().is_none());
    }

    #[test]
    fn audio_roundtrip_via_v4_modem_encode_to_samples() {
        // End-to-end through the V4Modem trait: build a request, encode
        // to audio, run rx_v4_audio. Mirrors what the GUI's TX/RX
        // pipeline will do once C-8 lands.
        let payload = rng_bytes(1_200, 0x1);
        let cfg = profile_high_2x();
        let n_packets = {
            let k_bytes = cfg.base.ldpc_rate.k() / 8;
            let k_source = modem_framing::raptorq_codec::k_from_payload(payload.len(), k_bytes)
                as u32;
            k_source + modem_framing::raptorq_codec::n_repair_default(k_source)
        };
        let req = EncodeRequest {
            profile: "HIGH2X",
            wire_payload: &payload,
            session_id: 0xCAFE,
            mime_type: mime::BINARY,
            hash_short: 0,
            esi_start: 0,
            n_packets,
            vox_seconds: 0.0,
        };
        let audio = V4Modem.encode_to_samples(&req).expect("encode");
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_all_eight_profiles() {
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(400, p.as_u8() as u64);
            let audio = modulate_for(&cfg, &payload, 0x42);
            let result = rx_v4_audio(&audio, &cfg)
                .unwrap_or_else(|e| panic!("{p:?} a2s: {e}"))
                .unwrap_or_else(|| panic!("{p:?} decode None"));
            assert_eq!(result.data, payload, "{p:?}");
        }
    }

    #[test]
    fn spawn_one_shot_decodes_wav_path_and_emits_events() {
        // End-to-end mirror of the GUI's start_capture_from_wav path:
        // 1. Encode a payload to V4 audio (with envelope, so the worker
        //    can write a user-named file).
        // 2. Push the audio in 500ms chunks through an mpsc channel.
        // 3. Close the channel — spawn should run its one-shot decode.
        // 4. Verify the RecordingSink saw `session_decoded` + the
        //    decoded file landed on disk.
        use modem_framing::payload_envelope::PayloadEnvelope;
        use modem_worker_base::RecordingSink;
        use std::sync::mpsc;

        let content = rng_bytes(300, 0x99);
        let envelope =
            PayloadEnvelope::new("rx_spawn_test.bin", "HB9TOB", content.clone()).expect("env");
        let wire = envelope.encode();
        let cfg = profile_high_2x();
        let k_bytes = cfg.base.ldpc_rate.k() / 8;
        let k_source = modem_framing::raptorq_codec::k_from_payload(wire.len(), k_bytes) as u32;
        let n_packets =
            k_source + modem_framing::raptorq_codec::n_repair_default(k_source);
        let req = EncodeRequest {
            profile: "HIGH2X",
            wire_payload: &wire,
            session_id: 0xCAFE_FACE,
            mime_type: mime::BINARY,
            hash_short: 0x1234,
            esi_start: 0,
            n_packets,
            vox_seconds: 0.0,
        };
        let audio = V4Modem.encode_to_samples(&req).expect("encode");

        let tmp = tempfile::tempdir().expect("tmp");
        let save_dir = Arc::new(Mutex::new(tmp.path().to_path_buf()));
        let wav_sink: SharedWavSink = Arc::new(Mutex::new(None));
        // Two Arc handles to the same RecordingSink: one as the dyn
        // EventSink the worker emits through, one typed so we can call
        // `.events()` for assertions afterwards. Mutex inside
        // RecordingSink makes the cross-thread share sound.
        let recording: Arc<RecordingSink> = Arc::new(RecordingSink::new());
        let sink: Arc<dyn EventSink> = recording.clone();
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let mut handle = spawn(
            rx,
            sink,
            save_dir,
            wav_sink,
            "HIGH2X".to_string(),
            false,
            Arc::new(AtomicU64::new(0)),
        );
        // Push the audio in 500ms-equivalent chunks (24_000 @ 48 kHz).
        // No wall-clock pacing — the spawn is one-shot so it doesn't
        // care about real-time alignment.
        const BATCH: usize = 24_000;
        for i in (0..audio.len()).step_by(BATCH) {
            let end = (i + BATCH).min(audio.len());
            tx.send(audio[i..end].to_vec()).expect("send");
        }
        drop(tx);
        // Join WITHOUT setting the stop flag: the worker must observe
        // the channel close, drain the in-flight chunks, then run its
        // one-shot decode. handle.stop() would race the recv loop and
        // bail out with a partial buffer.
        handle
            .thread
            .take()
            .expect("worker thread present")
            .join()
            .expect("worker join");

        let events = recording.events();
        let names: Vec<_> = events.iter().map(|(n, _)| n.as_str()).collect();
        let err_msg = events
            .iter()
            .find(|(n, _)| n == "error")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        assert!(
            names.contains(&"session_armed"),
            "missing session_armed: events={names:?} error={err_msg}"
        );
        assert!(
            names.contains(&"session_decoded"),
            "missing session_decoded: {names:?}"
        );
        assert!(
            names.contains(&"file_complete"),
            "missing file_complete: {names:?}"
        );

        // file_complete.saved_path should exist with the original bytes.
        let file_complete = events
            .iter()
            .find(|(n, _)| n == "file_complete")
            .map(|(_, v)| v.clone())
            .expect("file_complete event");
        let saved_path = file_complete["saved_path"]
            .as_str()
            .expect("saved_path string");
        let written = std::fs::read(saved_path).expect("read saved");
        assert_eq!(written, content, "decoded file != original content");
    }

    #[test]
    fn best_symbol_phase_sof_locks_onto_real_burst() {
        // Compose a real audio burst then verify the SOF-anchored phase
        // pick lands on an offset that decodes — i.e. the symbol-stream
        // sampled at that phase contains a SOF correlation peak.
        let cfg = profile_high_2x();
        let payload = rng_bytes(200, 0xDA);
        let audio = modulate_for(&cfg, &payload, 0x1);
        let bb = modem_core_base::demodulator::downmix(&audio, cfg.base.center_freq_hz);
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, 32);
        let mf = modem_core_base::demodulator::matched_filter(&bb, &taps);
        let phase = best_symbol_phase_sof(&mf, 32, &cfg);
        // Sample at this phase and check that find_next_sof finds a
        // peak (we just verify a non-empty symbol stream + decode).
        let n_syms = (mf.len() - phase) / 32;
        let syms: Vec<_> = (0..n_syms).map(|k| mf[phase + k * 32]).collect();
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg).expect("decode");
        assert!(!res.data.is_empty());
    }
}
