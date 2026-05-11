//! V3 worker - sliding-window RX over the session_buffer.
//!
//! Two paths run in parallel on the same accumulated audio buffer:
//!
//! 1. **Main loop**: as soon as we detect >= 2 preambles in the buffer, the
//!    window `[P_i - margin .. P_{i+1} + margin]` is processed with
//!    `rx_v2` as if it were a self-contained mini V2 transmission (timing
//!    re-init, FFE LS re-train, grid ppm, marker walk, LDPC decode). The
//!    decoded codewords are merged first-wins by ESI into a global
//!    accumulator. Each position `P_i` is finalized only once.
//!
//! 2. **Light tick** (every 1 s): `rx_v2` runs on the open window
//!    `[P_last - margin .. buffer_end]`. Its output is "provisional" and
//!    only refreshes the GUI progress events; as soon as a new preamble
//!    appears (-> window closes), the main loop re-decodes the full
//!    window cleanly.
//!
//! End of session: RMS silence >= 2 s after at least one preamble seen
//! -> final `rx_v2` on `[P_last - margin .. EOF]` -> `file_complete` ->
//! accumulator reset.
//!
//! The modem profile (constellation / LDPC rate / symbol rate) is passed
//! to `spawn()` when the capture starts. Changing profile requires a
//! stop/start of the worker.

use hound::{SampleFormat, WavSpec, WavWriter};
use modem_core::header::Header;
use modem_framing::payload_envelope::PayloadEnvelope;
use modem_core::profile::{ModemConfig, ProfileIndex};
use modem_core::rx_v2;
use modem_sdr_dsp::emphasis::DeemphasisFilter;
use modem_core::types::AUDIO_RATE;
use serde::Serialize;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::event_sink::{EventSink, EventSinkExt};
use crate::session_store::{self, SessionStore};

type WavFileWriter = WavWriter<BufWriter<std::fs::File>>;

/// Open WAV file + running sample counter. The worker writes into this while
/// `SharedWavSink` holds a `Some`; the Tauri start/stop commands create and
/// finalize it.
pub struct WavSink {
    writer: WavFileWriter,
    pub path: PathBuf,
    pub samples_written: u64,
}

impl WavSink {
    /// Create a new 48 kHz mono 16-bit WAV at `path`.
    pub fn create(path: &Path) -> Result<Self, hound::Error> {
        let spec = WavSpec {
            channels: 1,
            sample_rate: AUDIO_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer = WavWriter::create(path, spec)?;
        Ok(Self {
            writer,
            path: path.to_path_buf(),
            samples_written: 0,
        })
    }

    fn write_chunk(&mut self, samples: &[f32]) {
        for &s in samples {
            let val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            let _ = self.writer.write_sample(val);
        }
        self.samples_written += samples.len() as u64;
    }

    /// Flush + write header size. Consumes self.
    pub fn finalize(self) -> Result<(PathBuf, u64), hound::Error> {
        let samples = self.samples_written;
        let path = self.path.clone();
        self.writer.finalize()?;
        Ok((path, samples))
    }
}

/// Shared raw-capture sink. None = not recording ; Some = worker is teeing
/// every ingested batch into the WAV.
pub type SharedWavSink = Arc<Mutex<Option<WavSink>>>;

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("nbfm-worker.log")
}

fn worker_log(msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{msg}");
    }
}

// ---------------------------------------------------------------------------
// Event payloads (shared with the frontend listeners)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct PreamblePayload {
    profile: String,
    offset_samples: usize,
    offset_seconds: f64,
}

#[derive(Debug, Clone, Serialize)]
struct HeaderPayload {
    profile: String,
    mode_code: u8,
    payload_length: u16,
}

#[derive(Debug, Clone, Serialize)]
struct AppHeaderPayload {
    session_id: u32,
    file_size: u32,
    mime_type: u8,
    hash_short: u16,
}

#[derive(Debug, Clone, Serialize)]
struct EnvelopePayload {
    filename: String,
    callsign: String,
}

#[derive(Debug, Clone, Serialize)]
struct V2ProgressPayload {
    blocks_converged: usize,
    blocks_total: usize,
    blocks_expected: usize,
    /// Pilot-residual σ² (kept for backward compat / debug). The GUI
    /// surfaces `sigma2_data` instead, which excludes pilot+preamble
    /// overhead. See RxV2Result for the full definition.
    sigma2: f64,
    /// Hard-decision data-symbol σ² for the current window (frame-only,
    /// instantaneous). What the top bar and phase-error overlay display.
    sigma2_data: f64,
    converged_bitmap: Vec<u8>,
    constellation_sample: Vec<[f32; 2]>,
    /// Pilot LS smoothed phases per segment (radians) for the most
    /// recent decoded window. Empty if no segment was decoded yet.
    /// Includes both META and DATA segments; use [`pilot_phase_is_meta`]
    /// to tell them apart.
    pilot_phase_segments: Vec<Vec<f32>>,
    /// Parallel to [`pilot_phase_segments`]: `true` if the same-index
    /// entry is a META segment (header replicated), `false` for a
    /// regular DATA segment. The frontend uses this to colour META
    /// distinctly so the operator sees the full frame layout rather
    /// than only data segments.
    pilot_phase_is_meta: Vec<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct FileCompletePayload {
    filename: String,
    callsign: String,
    mime_type: u8,
    saved_path: String,
    /// Pilot-residual σ² of the last contributing window (legacy).
    sigma2: f64,
    /// Mean data-symbol σ² across every decode tick that contributed
    /// to this session. Surfaced in the GUI file panel as "moyenne".
    sigma2_data_avg: f64,
    size: usize,
}

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
struct SessionProgressPayload {
    session_id: u32,
    received: u32,
    needed: u32,
    decoded: bool,
    cap_reached: bool,
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
struct ErrorPayload {
    message: String,
}

// TX-overdrive detection : see previous worker version for calibration.
const OVERDRIVE_RMS_GATE_LINEAR: f32 = 0.056;
const OVERDRIVE_CREST_GATE_DB: f32 = 8.5;

#[derive(Debug, Clone, Serialize)]
struct AudioLevelPayload {
    rms: f32,
    peak: f32,
    total_samples: u64,
    overdrive: bool,
    crest_db: f32,
}

/// Real-time margin telemetry. Emitted every ~2 s so the GUI can show a
/// "RX surcharge CPU" badge when the worker can't keep up with the
/// capture (= classic Pi4 / old-PC sound-card symptom). Healthy systems
/// stay near `lag_ms = 0`, `last_batch_ms` ≪ 500, `dropped_samples = 0`.
///
/// `dropped_samples` is the cumulative count of f32 samples that the
/// bounded `cpal_capture` mpsc dropped at the source because the worker
/// couldn't drain fast enough (`SyncSender::try_send` returned `Full`).
/// SDR backends pre-buffer in-thread before sending and never trip this
/// counter, so on Pluto/SDRplay this stays 0 even on overloaded CPUs.
#[derive(Debug, Clone, Serialize)]
struct RxRealtimePayload {
    /// `(wall_clock - audio_consumed) * 1000`. Positive growing = falling
    /// behind. < 100 ms is healthy; > 100 ms surfaces a warning chip.
    lag_ms: f64,
    /// Wall time spent in the last batch (ingest + scan + decode). Batch
    /// is ~500 ms of audio, so any value > 500 means "this batch alone
    /// fell behind realtime".
    last_batch_ms: f64,
    /// Worst batch wall time over the last 2 s window. Resets on each
    /// emit so the chart shows local peaks.
    max_batch_ms: f64,
    /// Current `session_buffer.len() / 48 kHz` in ms. Bounded by
    /// `SESSION_HARD_CAP_SECONDS` while active, by `PREROLL_SECONDS`
    /// otherwise.
    session_buf_ms: f64,
    /// Cumulative samples dropped by the cpal capture mpsc since the
    /// worker started. Monotonic. The frontend computes the delta to
    /// flag fresh drops.
    dropped_samples: u64,
}

// ---------------------------------------------------------------------------
// Worker handle / spawn
// ---------------------------------------------------------------------------

pub struct WorkerHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Spawn an RX worker.
///
/// If `forced=true`, the RX profile is locked on `profile`:
/// - the FFT gate (family auto-detection) is fully bypassed,
/// - the post-decode header refine is bypassed as well.
/// This is mandatory to decode EXPERIMENTAL profiles (HIGH+, FAST) which
/// are absent from `PROBE_TEMPLATES` and therefore not auto-detected.
/// Spawn the V3 RX worker. `dropped_samples` is the cumulative count of
/// f32 samples that the cpal-capture bounded mpsc has dropped because
/// the worker couldn't drain fast enough; SDR backends pass a fresh
/// always-zero counter (their producers pre-buffer in-thread). Read
/// every 2 s by the realtime tick and surfaced as
/// `rx_realtime.dropped_samples` to the GUI.
pub fn spawn(
    samples: Receiver<Vec<f32>>,
    sink: Arc<dyn EventSink>,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    profile: ProfileIndex,
    forced: bool,
    deemphasis_enabled: bool,
    dropped_samples: Arc<std::sync::atomic::AtomicU64>,
) -> WorkerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        run_worker(
            samples,
            sink,
            save_dir,
            wav_sink,
            profile,
            forced,
            deemphasis_enabled,
            dropped_samples,
            stop_thread,
        );
    });
    WorkerHandle {
        stop,
        thread: Some(thread),
    }
}

// ---------------------------------------------------------------------------
// Tuning constants
// ---------------------------------------------------------------------------

/// Drain audio in ~500 ms batches (24 000 samples @ 48 kHz) to amortise the
/// per-batch overhead and bound the scan/tick frequency.
const BATCH_TARGET_SAMPLES: usize = 24_000;

/// Re-scan `find_all_preambles` + route to store at most every this many ms.
const SCAN_INTERVAL_MS: u64 = 1000;

/// RMS threshold below which we consider the channel silent (updates the
/// last_audio_above_silence_at heuristic, informational only now that the
/// session doesn't "close" on silence).
const SILENCE_RMS_THRESHOLD: f32 = 0.005;

/// Trim the in-memory audio buffer after this many seconds of active
/// session — the disk store is unaffected and retains all packets.
const MAX_SESSION_SECONDS: u64 = 25 * 60;

/// Amount of audio kept in the in-memory buffer after a burst ends (EOT
/// received, or fountain decode succeeded). Leaves enough context for a new
/// preamble already partially landing in this tick to still be detected.
const PREROLL_SECONDS: usize = 2;

// History (kept terse) :
//   - `CAPTURE_WINDOW_SECONDS = 15` rolling trim (data-loss cascade
//     on long HIGH+ when a slow rx_v3 tick + trim dropped pending
//     audio). Replaced by an unbounded buffer + per-tick scan
//     bounding.
//   - `SCAN_WINDOW_FAST/ULTRA_SECONDS` (per-tick fixed scan window,
//     4 s for fast / 12 s for ULTRA). Failed because the V3 preamble
//     period is ~4 s for *every* profile (`V3_PREAMBLE_PERIOD_S`),
//     so a 4 s scan window hit the boundary case where 1 of every
//     ~4 ticks just barely missed having two preambles
//     simultaneously visible — that period's superframe never got a
//     full-grid `rx_v2` decode and lost the codewords that needed
//     drift compensation. Symptom: ~25 % block loss at low CPU.
//   - **Now** : the scan walks the *entire* `session_buffer`, and
//     after every successful tick the worker drains everything before
//     `last_preamble_offset - TRUNCATE_MARGIN` (the position of the
//     last preamble found by `find_all_preambles`, returned by
//     `rx_v3_after`). The buffer becomes a self-purging queue that
//     tracks the live preamble cadence — no period guess, no fixed
//     scan window. P_last itself is preserved so the next tick can
//     re-decode `[P_last, P_last+1]` as a closed window with the
//     full drift grid once the next preamble lands.

/// Pre-roll preserved before `last_preamble_offset` when truncating the
/// session buffer after a scan. Covers `rx_v3_after`'s matched-filter
/// pre-roll context (`(RRC_SPAN_SYM + 4) * pitch` ≤ 1536 samples = 32 ms
/// for the slowest profile, ULTRA at sps=96), with margin. 100 ms is
/// plenty for any V3 profile and costs ~4800 f32 = 19 KB of RAM.
const TRUNCATE_MARGIN_MS: usize = 100;

/// Hard cap on the in-memory audio buffer during a session. Defends
/// against pathological cases where the truncation loop never advances
/// (no preamble ever found, profile-detect loop, etc.). Five minutes at
/// 48 kHz f32 mono = ~57 MB; below the practical RAM budget on any
/// target. Bursts longer than this are unsupported by the V3 modem
/// anyway (`MAX_SESSION_SECONDS`).
const SESSION_HARD_CAP_SECONDS: usize = 5 * 60;

/// Fall back to Idle if no preamble has been seen for this long while
/// Capturing — covers the case where the sender disappears mid-burst without
/// sending an EOT.
const PREAMBLE_SILENCE_TIMEOUT_S: u64 = 6;

// ---------------------------------------------------------------------------
// Worker state
// ---------------------------------------------------------------------------

struct WorkerState {
    config: ModemConfig,
    profile: ProfileIndex,
    /// `true` when the RX profile is locked by the user (UI:
    /// "Forcer un profil"). Disables auto-detection (FFT gate) and the
    /// post-decode header refine. Required for HIGH+/FAST.
    forced: bool,
    /// Accumulated audio for the current capture. Rolled to PREROLL_SECONDS
    /// while Idle (cheap noise buffer) ; bounded to SESSION_HARD_CAP_SECONDS
    /// while Capturing so rx_v3 stays fast even on long salves.
    session_buffer: Vec<f32>,
    /// Disk-persistent store of decoded codewords, per session_id.
    store: SessionStore,
    /// session_ids already announced to the UI (emit `session_armed` once).
    announced_sessions: HashSet<u32>,
    /// Last `received` count emitted per session, for progress rate-limiting.
    last_progress: std::collections::HashMap<u32, u32>,
    /// Running mean of `result.sigma2_data` per session (sum + count).
    /// Accumulated on every successful decode tick that touches the
    /// session, drained on `file_complete` so the GUI receives the
    /// across-the-burst average rather than the last window's value.
    sigma2_data_running: std::collections::HashMap<u32, (f64, u32)>,
    /// First decoded protocol header, for legacy `header` event emission.
    header: Option<Header>,
    last_scan_at: Instant,
    last_audio_above_silence_at: Instant,
    /// True once we've seen a valid preamble — Idle vs Capturing phase flag.
    session_active: bool,
    session_started_at: Instant,
    /// Last time a preamble was confirmed via rx_v3 (== last tick that
    /// produced an app_header). Used to fall back to Idle when the sender
    /// disappears mid-burst without sending EOT.
    last_preamble_seen_at: Instant,
    total_samples: u64,
    /// Optional NBFM de-emphasis filter applied to the path going to the
    /// modem demodulator (after the raw WAV tee + level meter). `None`
    /// when the user has not enabled the toggle in Settings.
    deemphasis: Option<DeemphasisFilter>,
}

impl WorkerState {
    fn new(
        profile: ProfileIndex,
        forced: bool,
        store: SessionStore,
        deemphasis_enabled: bool,
    ) -> Self {
        let now = Instant::now();
        Self {
            config: profile.to_config(),
            profile,
            forced,
            session_buffer: Vec::new(),
            store,
            announced_sessions: HashSet::new(),
            last_progress: std::collections::HashMap::new(),
            sigma2_data_running: std::collections::HashMap::new(),
            header: None,
            last_scan_at: now,
            last_audio_above_silence_at: now,
            session_active: false,
            session_started_at: now,
            last_preamble_seen_at: now,
            total_samples: 0,
            deemphasis: deemphasis_enabled.then(DeemphasisFilter::new),
        }
    }

    fn soft_reset_buffer(&mut self) {
        self.session_buffer.clear();
        self.header = None;
        self.session_active = false;
        self.announced_sessions.clear();
    }

    /// Keep only the last `PREROLL_SECONDS` of audio in the in-memory buffer.
    /// Called when the current burst has ended (EOT or fountain decode) so
    /// that subsequent rx_v3 scans don't re-process a growing trailing tail,
    /// but a leading edge of a new preamble that might already be landing in
    /// this tick isn't lost.
    fn trim_buffer_to_preroll(&mut self) {
        let keep = AUDIO_RATE as usize * PREROLL_SECONDS;
        let len = self.session_buffer.len();
        if len > keep {
            self.session_buffer.drain(..len - keep);
        }
        self.header = None;
        self.session_active = false;
        self.announced_sessions.clear();
    }
}

// ---------------------------------------------------------------------------
// Worker main
// ---------------------------------------------------------------------------

fn run_worker(
    samples: Receiver<Vec<f32>>,
    sink: Arc<dyn EventSink>,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    profile: ProfileIndex,
    forced: bool,
    deemphasis_enabled: bool,
    dropped_samples: Arc<std::sync::atomic::AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let _ = std::fs::remove_file(log_path());
    worker_log(&format!(
        "[worker] start V3 profile={:?} forced={}",
        profile, forced
    ));

    // Initialise the disk-persistent session store. Expired sessions (> 24 h)
    // are dropped on construction.
    let sessions_root = save_dir.lock().map(|g| g.clone()).unwrap_or_default();
    let store = match SessionStore::new(&sessions_root) {
        Ok(s) => s,
        Err(e) => {
            worker_log(&format!("[worker] session store init failed: {e}"));
            return;
        }
    };
    let mut state = WorkerState::new(profile, forced, store, deemphasis_enabled);

    // Telemetry to surface "worker falling behind realtime" — the
    // signature of CPU-limited HIGH+ on the Pi 5 SDR path. We compare
    // wall-clock time to audio time (= samples processed / 48 kHz);
    // on a healthy system the two stay locked. A growing gap is the
    // smoking gun that the decoder can't keep up with the capture.
    let started = Instant::now();
    let mut last_telemetry_tick = Instant::now();
    let mut last_batch_processing_ms: f64 = 0.0;
    let mut max_batch_processing_ms: f64 = 0.0;
    // Cumulative `dropped_samples` reading observed last loop iteration.
    // Any positive delta means the capture-side ring overflowed since
    // we last looked — see brickwall handler below.
    let mut last_dropped: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        let first = match samples.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => {
                // Idle : still pulse the maintenance checks (silence trigger)
                maintenance_tick(&*sink, &save_dir, &mut state);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let batch_start = Instant::now();
        let mut batch = first;
        batch.reserve(BATCH_TARGET_SAMPLES);
        while batch.len() < BATCH_TARGET_SAMPLES {
            match samples.try_recv() {
                Ok(more) => batch.extend_from_slice(&more),
                Err(_) => break,
            }
        }

        state.total_samples += batch.len() as u64;

        // Capture-side brickwall: did the OS-audio 30 s ring overflow
        // since the last iteration? If yes and we're mid-session, the
        // session_buffer now contains a hole (the dropped samples are
        // gone; the audio downstream of the gap doesn't line up with
        // what came before for the matched filter / drift estimator).
        // Discard the in-flight session and return to idle so the next
        // preamble is treated as a clean start, mirroring what the user
        // wants: "Si un des brickwall est atteint on flush tout et on
        // repart un idle pour attendre la prochaine superframe."
        //
        // While idle, an overflow is informational — we drop the current
        // batch (would be discarded by the preroll trim anyway) and
        // continue without flushing.
        let cur_dropped = dropped_samples.load(Ordering::Relaxed);
        if cur_dropped > last_dropped {
            let delta = cur_dropped - last_dropped;
            last_dropped = cur_dropped;
            if state.session_active {
                worker_log(&format!(
                    "[worker] BRICKWALL capture: +{delta} mono-samples dropped \
                     during active session → flush + idle"
                ));
                state.soft_reset_buffer();
                let flushed = drain_mpsc(&samples, &mut state.total_samples);
                if flushed > 0 {
                    worker_log(&format!(
                        "[worker] flushed {flushed} mpsc samples on brickwall"
                    ));
                }
                // Skip the rest of this iteration — the batch we just
                // pulled is itself part of the corrupted stream.
                continue;
            } else {
                worker_log(&format!(
                    "[worker] ring overflow +{delta} mono-samples while idle (no flush)"
                ));
            }
        }

        // Raw capture (if armed)
        if let Ok(mut guard) = wav_sink.lock() {
            if let Some(ref mut sink) = *guard {
                sink.write_chunk(&batch);
            }
        }

        // Audio level metrics + silence tracker
        let (peak, rms, crest_db) = compute_audio_stats(&batch);
        let overdrive =
            rms > OVERDRIVE_RMS_GATE_LINEAR && crest_db < OVERDRIVE_CREST_GATE_DB;
        sink.emit(
            "audio_level",
            AudioLevelPayload {
                rms,
                peak,
                total_samples: state.total_samples,
                overdrive,
                crest_db,
            },
        );
        if rms > SILENCE_RMS_THRESHOLD {
            state.last_audio_above_silence_at = Instant::now();
        }

        // Optional NBFM de-emphasis. Applied AFTER the raw WAV tee and the
        // audio_level metric so those still reflect what the radio actually
        // delivered (clip diagnostics stay meaningful), and BEFORE feeding
        // the modem demodulator so rx_v3 sees a flat spectrum when paired
        // with a transmitter that ran the matching pre-emphasis.
        if let Some(filter) = state.deemphasis.as_mut() {
            filter.process(&mut batch);
        }

        // Append. Trim policy:
        //   - Idle  : PREROLL_SECONDS rolling window — just enough for a
        //             preamble landing across batch boundaries.
        //   - Active: NO rolling trim from the ingest path. Truncation
        //             happens in `scan_and_route` after each successful
        //             rx_v3 tick, draining everything before
        //             `last_preamble_offset - TRUNCATE_MARGIN`. The
        //             buffer therefore tracks the live preamble cadence
        //             rather than a fixed-size window — closed windows
        //             are always intact, no in-flight audio is ever
        //             dropped under the decoder.
        //   - Both  : SESSION_HARD_CAP_SECONDS catches pathological
        //             "session never closes / no preamble ever found"
        //             cases (sender vanished, profile-detect loop, ...).
        state.session_buffer.extend_from_slice(&batch);
        let len = state.session_buffer.len();
        if state.session_active {
            // Worker-side brickwall. SESSION_HARD_CAP_SECONDS (5 min) is
            // the proxy for "the worker has accumulated 5 minutes of
            // audio without scan_and_route ever managing to truncate
            // it" — either no preamble ever found, profile-detect loop,
            // or the worker is so far behind realtime that the buffer
            // builds up faster than it can be drained. In all these
            // cases the in-flight decode state is unrecoverable;
            // flush + idle so the next preamble lands on a clean buffer.
            // Same drain_mpsc semantic as the capture brickwall.
            let cap = AUDIO_RATE as usize * SESSION_HARD_CAP_SECONDS;
            if len > cap {
                worker_log(&format!(
                    "[worker] BRICKWALL worker: session_buffer reached {:.0}s without \
                     scan_and_route truncation → flush + idle",
                    len as f64 / AUDIO_RATE as f64,
                ));
                state.soft_reset_buffer();
                let flushed = drain_mpsc(&samples, &mut state.total_samples);
                if flushed > 0 {
                    worker_log(&format!(
                        "[worker] flushed {flushed} mpsc samples on brickwall"
                    ));
                }
                continue;
            }
        } else {
            // Idle: keep a rolling PREROLL_SECONDS noise buffer so a
            // preamble landing across batch boundaries is still
            // detectable. Drop-from-the-front is harmless here — no
            // decode is in flight.
            let cap = AUDIO_RATE as usize * PREROLL_SECONDS;
            if len > cap {
                state.session_buffer.drain(..len - cap);
            }
        }

        maintenance_tick(&*sink, &save_dir, &mut state);

        // Per-batch wall time, tracked so the 2 s telemetry tick can
        // surface peak processing cost. A batch ≈ 500 ms of audio,
        // so anything over ~500 ms here is "falling behind realtime".
        last_batch_processing_ms = batch_start.elapsed().as_secs_f64() * 1000.0;
        if last_batch_processing_ms > max_batch_processing_ms {
            max_batch_processing_ms = last_batch_processing_ms;
        }

        // 2 s realtime-margin tick.
        //   wall_s : seconds of wall clock since worker started
        //   audio_s: seconds of audio that have actually been
        //            consumed by this loop (= total_samples / 48 kHz)
        //   lag_ms : (wall_s - audio_s) * 1000. Ideally near 0. A
        //            positive growing value means the decoder can't
        //            keep up with the capture and the session_buffer
        //            trim is silently dropping audio under us.
        if last_telemetry_tick.elapsed() >= Duration::from_secs(2) {
            let wall_s = started.elapsed().as_secs_f64();
            let audio_s = state.total_samples as f64 / AUDIO_RATE as f64;
            let lag_ms = (wall_s - audio_s) * 1000.0;
            let session_buf_ms =
                state.session_buffer.len() as f64 * 1000.0 / AUDIO_RATE as f64;
            let dropped = dropped_samples.load(Ordering::Relaxed);
            worker_log(&format!(
                "[worker] tick: audio={audio_s:.1}s wall={wall_s:.1}s \
                 lag={lag_ms:+.0}ms last_batch={last_batch_processing_ms:.0}ms \
                 max_batch={max_batch_processing_ms:.0}ms \
                 session_buf={session_buf_ms:.0}ms dropped={dropped} \
                 session_active={}",
                state.session_active
            ));
            // Surface the metric to the GUI so a chip can flag CPU
            // overload in real time. Always emitted (every ~2 s) so the
            // frontend has fresh state to drive its own thresholding /
            // hysteresis (see `noteRxRealtime` in main.js).
            sink.emit(
                "rx_realtime",
                RxRealtimePayload {
                    lag_ms,
                    last_batch_ms: last_batch_processing_ms,
                    max_batch_ms: max_batch_processing_ms,
                    session_buf_ms,
                    dropped_samples: dropped,
                },
            );
            last_telemetry_tick = Instant::now();
            max_batch_processing_ms = 0.0;
        }
    }

    worker_log("[worker] stop");
}

/// Drain whatever the cpal-reader thread has queued in the mpsc and
/// account for it in `total_samples` so the post-flush `lag_ms` metric
/// is honest (those samples WERE observed by the worker, we just chose
/// not to decode them). Returns the number of f32 samples discarded —
/// purely for the log line.
///
/// Called on both brickwall paths (capture ring overflow and worker
/// 5 min cap) so we don't restart the next session on top of a stale
/// backlog.
fn drain_mpsc(samples: &Receiver<Vec<f32>>, total_samples: &mut u64) -> usize {
    let mut n = 0;
    while let Ok(chunk) = samples.try_recv() {
        *total_samples += chunk.len() as u64;
        n += chunk.len();
    }
    n
}

fn compute_audio_stats(batch: &[f32]) -> (f32, f32, f32) {
    let mut peak: f32 = 0.0;
    let mut sqsum: f64 = 0.0;
    for &s in batch {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sqsum += (s as f64) * (s as f64);
    }
    let rms = (sqsum / batch.len().max(1) as f64).sqrt() as f32;
    let crest_db = if peak > 1e-9 && rms > 1e-9 {
        20.0 * (peak / rms).log10()
    } else {
        0.0
    };
    (peak, rms, crest_db)
}

/// Runs periodically on every batch AND on idle timeouts. Handles :
///  - find_all_preambles + rx_v3 scan (throttled to SCAN_INTERVAL_MS)
///  - routes decoded packets to the disk-persistent session store
///  - max-duration guard on the in-memory audio buffer
fn maintenance_tick(
    sink: &dyn EventSink,
    save_dir: &Arc<Mutex<PathBuf>>,
    state: &mut WorkerState,
) {
    let now = Instant::now();

    if now.duration_since(state.last_scan_at) >= Duration::from_millis(SCAN_INTERVAL_MS) {
        state.last_scan_at = now;
        scan_and_route(sink, save_dir, state);
    }

    // Preamble-silence fallback : if we're Capturing but haven't seen a
    // confirmed preamble for PREAMBLE_SILENCE_TIMEOUT_S, the sender likely
    // vanished mid-burst (no EOT received). Drop back to Idle so the next
    // salve starts cleanly on a 2-s pre-roll rather than accumulating
    // staleness.
    if state.session_active {
        let since_preamble = now.duration_since(state.last_preamble_seen_at);
        if since_preamble >= Duration::from_secs(PREAMBLE_SILENCE_TIMEOUT_S) {
            worker_log("[worker] preamble silence timeout, returning to Idle");
            state.trim_buffer_to_preroll();
        }
    }

    // Hard cap : if a session has been "active" for more than
    // MAX_SESSION_SECONDS without the user stopping (stuck state, bug, etc.),
    // trim the audio buffer defensively. The disk store is unaffected and
    // keeps its packets.
    if state.session_active {
        let active_for = now.duration_since(state.session_started_at);
        if active_for >= Duration::from_secs(MAX_SESSION_SECONDS) {
            worker_log("[worker] audio buffer max duration reached, trimming");
            state.soft_reset_buffer();
        }
    }
}

// ---------------------------------------------------------------------------
// Main decoding path : rx_v3 scan → route decoded CWs to SessionStore
// ---------------------------------------------------------------------------

fn scan_and_route(
    sink: &dyn EventSink,
    save_dir: &Arc<Mutex<PathBuf>>,
    state: &mut WorkerState,
) {
    let buf_secs = state.session_buffer.len() as f64 / AUDIO_RATE as f64;

    // [perf] Phase-1 instrumentation : track ms spent per scan in the two
    // dominant cost-driver functions (detect_best_profile, rx_v3_after) and
    // the buffer's mean-squared energy. Always-on (Instant calls are cheap),
    // logged on the existing `[scan]` lines. Used to calibrate the Phase-2
    // energy gate. See plans/perf-rx-idle-surface-pro-7.md.
    let rms_sqr: f64 = if state.session_buffer.is_empty() {
        0.0
    } else {
        let sum: f64 = state
            .session_buffer
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum();
        sum / state.session_buffer.len() as f64
    };

    // [perf] Phase-2a/b — preamble-presence FFT probe gate. Runs in Idle only :
    // when the worker is already in a session the buffer is full of signal
    // by construction and the probe would always fire. Cost ≈ 25-30 ms vs
    // ~1000 ms for the full legacy pipeline.
    //
    // Distinct preamble sequences per `(sps, β)` family (Phase 2b — A: 32/0.20,
    // B: 48/0.25, C: 96/0.25) let the gate ALSO classify the on-air family in
    // a single pass. We use that family directly to pick a sensible anchor
    // profile, replacing `detect_best_profile` (~720 ms) entirely. Within
    // family A the (NORMAL/HIGH/MEGA) ambiguity is resolved by the protocol
    // header's `profile_index` byte — handled in the existing post-decode
    // refinement block below.
    let mut t_probe_us: u128 = 0;
    let mut probe_ratio: f64 = 0.0;
    let mut probe_label: String = String::from("-");
    if !state.forced && !state.session_active && !state.session_buffer.is_empty() {
        let probe = modem_core::gate::PreambleProbe::for_buf_len(state.session_buffer.len());
        let t0 = Instant::now();
        let r = probe.check(&state.session_buffer);
        t_probe_us = t0.elapsed().as_micros();
        probe_ratio = r.max_ratio;
        probe_label = format!("{:?}", r.best_anchor);
        if !r.passes(modem_core::gate::PROBE_THRESHOLD) {
            worker_log(&format!(
                "[scan] active=false buf={:.1}s gated profile={:?} rms_sqr={:.6} t_probe_us={} probe={}@{:.1} (<{})",
                buf_secs, state.profile, rms_sqr, t_probe_us, probe_label, probe_ratio, modem_core::gate::PROBE_THRESHOLD,
            ));
            return;
        }
        // Gate fired with an anchor profile. Switch state.profile when
        // ours doesn't match — except keep HIGH if the gate said NORMAL
        // (same pitch=32 family A, header still refines NORMAL→HIGH).
        let want_anchor = r.best_anchor;
        let already_aligned = state.profile == want_anchor
            || (state.profile == ProfileIndex::High && want_anchor == ProfileIndex::Normal);
        if !already_aligned {
            worker_log(&format!(
                "[auto-profile] gate picked anchor {:?} (was {:?})",
                want_anchor, state.profile
            ));
            state.profile = want_anchor;
            state.config = want_anchor.to_config();
        }
    }
    // Forced mode: no FFT gate (EXPERIMENTAL profiles are absent from
    // PROBE_TEMPLATES) and no preamble squelch. rx_v3 will run its own
    // find_preamble downstream - on pure noise it bails out quickly (None).

    // `t_detect_us` retained in the [scan] log for compatibility with
    // existing baseline captures ; phase-2b removes detect_best_profile
    // from the path so this is always 0 unless a future revision restores
    // a per-tick classifier here.
    let t_detect_us: u128 = 0;

    let config = state.config.clone();

    // Scan the entire `session_buffer` — no fixed-size scan window, no
    // skip_until watermark. The buffer is kept small by the post-scan
    // truncation below : after every successful tick we drain
    // everything before `last_preamble_offset - TRUNCATE_MARGIN`, so by
    // construction the buffer holds at most ~one preamble period of
    // already-walked audio (≤ TRUNCATE_MARGIN_MS) plus whatever has
    // arrived since the previous tick. `rx_v3_after`'s linear
    // downmix/MF cost therefore stays bounded by the live preamble
    // cadence, not by burst length.
    let t_rx_v3_start = Instant::now();
    // IDLE pre-activation → `finalize=true` (decode the trailing OPEN
    // window so its header can trigger session activation); once
    // ACTIVE → `finalize=false` (skip OPEN, wait for the next preamble
    // to close it on a subsequent tick — avoids the segment-by-segment
    // drift accumulation that pilot-only tracking suffers on an
    // unclosed window). `rx_v3_after` auto-escalates back to a decode
    // if a CLOSED window of this same buffer carries an EOT marker,
    // so end-of-burst recovery doesn't need a second scan.
    let finalize = !state.session_active;
    let rx_v3_opt = rx_v2::rx_v3_after(&state.session_buffer, &config, 0, finalize);
    let t_rx_v3_us = t_rx_v3_start.elapsed().as_micros();
    let Some(mut result) = rx_v3_opt else {
        worker_log(&format!(
            "[scan] active={} buf={:.1}s rx_v3=None profile={:?} rms_sqr={:.6} t_probe_us={} probe={}@{:.1} t_detect_us={} t_rx_v3_us={}",
            state.session_active, buf_secs, state.profile, rms_sqr, t_probe_us, probe_label, probe_ratio, t_detect_us, t_rx_v3_us
        ));
        return;
    };

    // Refine profile from the Golay-decoded header (disambiguates profiles
    // that share Rs/tau/beta — e.g. HIGH vs NORMAL — by reading their
    // canonical profile_index byte). On mismatch, switch the worker config
    // and immediately re-run rx_v3 on the SAME buffer with the corrected
    // profile, so the codewords just walked over with the wrong data
    // constellation are recovered. Without this re-decode, the in-memory
    // buffer is still in Idle trim mode (PREROLL = 2 s), so by the next
    // 1-Hz scan tick the leading edge of the 1st superframe is gone — the
    // user sees ESI 0..N missing, exactly the failure mode reported on
    // NORMAL OTA captures.
    if !state.forced {
        if let Some(ref hdr) = result.header {
            if let Some(hdr_profile) =
                modem_core::profile::ProfileIndex::from_u8(hdr.profile_index)
            {
                if hdr_profile != state.profile {
                    worker_log(&format!(
                        "[auto-profile] header says {:?} (was {:?}), re-decoding window",
                        hdr_profile, state.profile
                    ));
                    state.profile = hdr_profile;
                    state.config = hdr_profile.to_config();
                    state.header = None;
                    state.announced_sessions.clear();
                    sink.emit(
                        "profile_auto_detected",
                        serde_json::json!({ "profile": profile_name(state.profile) }),
                    );
                    let new_config = state.config.clone();
                    // Auto-profile re-decode: same `finalize` policy as
                    // the main tick. We got here BECAUSE the first
                    // decode produced a header (so a CLOSED window did
                    // decode) — `session_active` may still be false at
                    // this point and gets flipped further down based on
                    // the refreshed result. `!session_active` correctly
                    // forces the OPEN decode for the activation path.
                    let finalize = !state.session_active;
                    match rx_v2::rx_v3_after(&state.session_buffer, &new_config, 0, finalize) {
                        Some(refresh) => {
                            // Defensive : if the fresh header still claims a
                            // different profile, give up rather than loop.
                            let still_mismatched = refresh
                                .header
                                .as_ref()
                                .and_then(|h| {
                                    modem_core::profile::ProfileIndex::from_u8(h.profile_index)
                                })
                                .map(|p| p != state.profile)
                                .unwrap_or(false);
                            if still_mismatched {
                                worker_log(
                                    "[auto-profile] still mismatched after re-decode, giving up",
                                );
                                return;
                            }
                            result = refresh;
                        }
                        None => return,
                    }
                }
            }
        }
    } else if let Some(ref hdr) = result.header {
        // Forced mode: we do NOT switch profile, but we log mismatches to
        // help debugging ("peer sends HIGH while we're forced on HIGH+").
        if let Some(hdr_profile) = modem_core::profile::ProfileIndex::from_u8(hdr.profile_index) {
            if hdr_profile != state.profile {
                worker_log(&format!(
                    "[forced] header says {:?}, locked on {:?} -> expected decode failure",
                    hdr_profile, state.profile
                ));
            }
        }
    }
    let eot_seen = result.eot_seen;
    // `sigma2`      : pilot residual variance, includes meta segments
    //                 (header pilots count toward the average).
    // `sigma2_data` : data-symbol hard-decision residual variance,
    //                 EXCLUDES meta — closer to what the actual payload
    //                 constellation looks like.
    // Logging both so we can tell whether a high `sigma2` is real data
    // degradation (`sigma2_data` also high) vs a meta-pilot statistical
    // artifact (`sigma2_data` low). The 2026-05-11 sound-card investigation
    // hinges on this distinction.
    worker_log(&format!(
        "[scan] active={} buf={:.1}s rx_v3=Some hdr={} v{} flags=0x{:02X} ah={} cw_map={} conv={}/{} segs={}/{} sigma2={:.4} sigma2_data={:.4} rms_sqr={:.6} t_probe_us={} probe={}@{:.1} t_detect_us={} t_rx_v3_us={}",
        state.session_active,
        buf_secs,
        result.header.is_some(),
        result.header.as_ref().map(|h| h.version).unwrap_or(0),
        result.header.as_ref().map(|h| h.flags).unwrap_or(0),
        result.app_header.is_some(),
        result.cw_bytes_map.len(),
        result.converged_blocks,
        result.total_blocks,
        result.segments_decoded,
        result.segments_lost,
        result.sigma2,
        result.sigma2_data,
        rms_sqr,
        t_probe_us,
        probe_label,
        probe_ratio,
        t_detect_us,
        t_rx_v3_us,
    ));

    // Per-segment pilot sigma² breakdown. Surfaces whether the aggregate
    // `sigma2` is uniformly distributed across segments or concentrated
    // on a subset (= confirms or refutes the "odd segments alternate to
    // sigma2=0.5" observation reported on HighPlus sound-card capture).
    // Format: `M:` for meta, `D:` for data, in temporal order. Skipped
    // when no per-segment data exists (e.g. rx_v3=None path).
    if !result.pilot_sigma2_per_segment.is_empty() {
        let mut parts = String::with_capacity(
            result.pilot_sigma2_per_segment.len() * 12,
        );
        for (i, &s) in result.pilot_sigma2_per_segment.iter().enumerate() {
            let tag = if *result.pilot_phase_is_meta.get(i).unwrap_or(&false) {
                "M"
            } else {
                "D"
            };
            if i > 0 {
                parts.push(' ');
            }
            parts.push_str(&format!("{tag}:{s:.4}"));
        }
        worker_log(&format!("[scan-segs] {parts}"));
    }

    // Transition to Capturing as soon as a V3 protocol header is Golay-
    // decoded. Requiring app_header (meta LDPC) would deadlock : the Idle
    // buffer is trimmed to 2 s, which is too short to decode the meta CW
    // (needs ~0.7 s post-preamble including pilots). Golay(24,12) with 3
    // correctable bits per block is reliable enough to trust as a preamble
    // confirmation ; once active, the buffer grows freely (capped only
    // by SESSION_HARD_CAP_SECONDS, ~5 min) and subsequent ticks will
    // populate the meta.
    let header_ok = result
        .header
        .as_ref()
        .map(|h| h.version == modem_core::frame::HEADER_VERSION_V3)
        .unwrap_or(false);
    if !state.session_active
        && (header_ok || result.app_header.is_some() || !result.cw_bytes_map.is_empty())
    {
        state.session_active = true;
        state.session_started_at = Instant::now();
        state.last_audio_above_silence_at = Instant::now();
        state.last_preamble_seen_at = Instant::now();
        sink.emit(
            "preamble",
            PreamblePayload {
                profile: profile_name(state.profile),
                offset_samples: 0,
                offset_seconds: 0.0,
            },
        );
    }
    // Any V3 header seen counts as a live preamble. Update the silence
    // timer even when the meta LDPC hasn't converged yet, otherwise we
    // drop back to Idle mid-burst on hard-SNR profiles (16-APSK R3/4) and
    // trash the buffer we've been accumulating.
    if header_ok {
        state.last_preamble_seen_at = Instant::now();
    }

    // Legacy protocol header event (once per session).
    if state.header.is_none() {
        if let Some(h) = result.header.clone() {
            sink.emit(
                "header",
                HeaderPayload {
                    profile: profile_name(state.profile),
                    mode_code: h.mode_code,
                    payload_length: h.payload_length,
                },
            );
            state.header = Some(h);
        }
    }

    // Without an AppHeader we can't know which session the packets belong to.
    // We still honour the EOT flag if it was set — it tells us the TX ended
    // this burst, so we can free the in-memory audio buffer right away.
    let Some(ref ah) = result.app_header else {
        if eot_seen {
            state.trim_buffer_to_preroll();
        }
        return;
    };
    // A valid AppHeader decoded → a preamble is confirmed on-air. Reset the
    // preamble-silence timer.
    state.last_preamble_seen_at = Instant::now();

    // Announce the session once per session_id seen. If the session is
    // already decoded on disk (e.g. same file re-transmitted after an earlier
    // successful reception), re-emit session_decoded + file_complete from
    // the stored payload so the UI surfaces it again.
    let is_new_session = !state.announced_sessions.contains(&ah.session_id);
    if is_new_session {
        state.announced_sessions.insert(ah.session_id);
        let session_dir = state
            .store
            .root()
            .join(format!("{:08x}.session", ah.session_id));
        sink.emit(
            "session_armed",
            SessionArmedPayload {
                session_id: ah.session_id,
                k: ah.k_symbols as u32,
                t: ah.t_bytes,
                file_size: ah.file_size,
                mime_type: ah.mime_type,
                profile: profile_name(state.profile),
                session_dir: session_dir.to_string_lossy().into_owned(),
            },
        );
        // Also fire the legacy app_header event so existing UI keeps working.
        sink.emit(
            "app_header",
            AppHeaderPayload {
                session_id: ah.session_id,
                file_size: ah.file_size,
                mime_type: ah.mime_type,
                hash_short: ah.hash_short,
            },
        );
        if let Some(df) = state.store.peek_decoded(ah, state.profile) {
            // Re-announce of an already-decoded session : we don't have
            // a running mean (this is a peek, not a fresh decode), so
            // pass the current window's sigma2_data as a best-effort.
            emit_decoded_file(sink, save_dir, &df, result.sigma2, result.sigma2_data);
        }
    }

    // Accumulate the running mean of sigma2_data for this session : every
    // tick that produced an AppHeader (and therefore a valid `result`)
    // contributes one sample. Drained when the file completes (below).
    {
        let entry = state
            .sigma2_data_running
            .entry(ah.session_id)
            .or_insert((0.0, 0));
        entry.0 += result.sigma2_data;
        entry.1 += 1;
    }

    // Route the packets to the disk store.
    let outcome = state
        .store
        .accept_packets(ah, state.profile, &result.cw_bytes_map);

    // Rate-limit : emit session_progress only when the received count
    // actually moves or the decoded flag changes.
    let last = state.last_progress.get(&ah.session_id).copied().unwrap_or(u32::MAX);
    if outcome.unique_esis != last || outcome.decoded.is_some() {
        state.last_progress.insert(ah.session_id, outcome.unique_esis);
        sink.emit(
            "session_progress",
            SessionProgressPayload {
                session_id: ah.session_id,
                received: outcome.unique_esis,
                needed: outcome.needed,
                decoded: outcome.decoded.is_some(),
                cap_reached: outcome.cap_reached,
            },
        );
        // Legacy v2_progress : cumulative bitmap from the disk-persistent
        // store (not the sliding rx_v3 window, which would only show the
        // last few seconds of ESIs and appear to "scroll").
        let sigma2 = result.sigma2;
        let sigma2_data = result.sigma2_data;
        let expected = outcome.needed as usize;
        sink.emit(
            "v2_progress",
            V2ProgressPayload {
                blocks_converged: outcome.unique_esis as usize,
                blocks_total: result.total_blocks,
                blocks_expected: expected,
                sigma2,
                sigma2_data,
                converged_bitmap: outcome.seen_bitmap.clone(),
                constellation_sample: result.constellation_sample.clone(),
                pilot_phase_segments: result.pilot_phase_segments.clone(),
                pilot_phase_is_meta: result.pilot_phase_is_meta.clone(),
            },
        );
    }

    // A freshly-decoded file : emit session_decoded, copy to save_dir root
    // under the envelope filename, and emit the legacy file_complete event.
    // The `sigma2_data` attached to file_complete is the running mean over
    // every tick that contributed to this session.
    if let Some(df) = outcome.decoded {
        let avg_sigma2_data = state
            .sigma2_data_running
            .get(&df.session_id)
            .map(|&(sum, n)| if n > 0 { sum / n as f64 } else { result.sigma2_data })
            .unwrap_or(result.sigma2_data);
        emit_decoded_file(sink, save_dir, &df, result.sigma2, avg_sigma2_data);
    }

    // Free the in-memory audio buffer only once the TX explicitly signalled
    // EOT. The older `just_decoded` trigger pre-dates the EOT frame : at
    // the time, early trim was the cheapest way to keep rx_v3 fast after
    // a decode. With EOT in place it becomes harmful — on a repair-padded
    // burst (pct > 0), convergence fires at K while the tail repair
    // packets are still on the wire, and the 2-s preroll usually strips
    // the last periodic preamble so those tail ESIs never get latched in
    // the next scan. The post-tick truncation below keeps the buffer
    // small without dropping in-flight repair packets.
    if eot_seen {
        state.trim_buffer_to_preroll();
        let _ = session_store::BLOB_WARN_RATIO;
        return;
    }

    // Self-purging queue : drain everything before the LAST preamble
    // found in this scan, minus a small MF pre-roll margin. P_last
    // itself is preserved so that the next tick sees it again — once
    // the next preamble (P_last+1) lands, the closed window
    // [P_last, P_last+1] gets a full-grid `rx_v2` decode. Without
    // this, the buffer would either grow without bound (memory) or
    // need a fixed-size scan window (which fails when scan_window
    // ≈ V3_PREAMBLE_PERIOD_S, see history note above the constants).
    if let Some(p_last) = result.last_preamble_offset {
        let margin = AUDIO_RATE as usize * TRUNCATE_MARGIN_MS / 1000;
        let drain_end = p_last
            .saturating_sub(margin)
            .min(state.session_buffer.len());
        if drain_end > 0 {
            state.session_buffer.drain(..drain_end);
        }
    }

    let _ = session_store::BLOB_WARN_RATIO; // keep the import visible for future UI use
}

/// Emit the `envelope` + `session_decoded` + `file_complete` events for a
/// decoded session and drop the envelope content in the root save_dir under
/// the sender's filename. Shared between the fresh-decode path and the
/// re-announce path (peek_decoded on a session that was already decoded in a
/// previous capture episode).
fn emit_decoded_file(
    sink: &dyn EventSink,
    save_dir: &Arc<Mutex<PathBuf>>,
    df: &session_store::DecodedFile,
    sigma2: f64,
    sigma2_data_avg: f64,
) {
    if let Some(fname) = df.meta.filename.clone() {
        sink.emit(
            "envelope",
            EnvelopePayload {
                filename: fname,
                callsign: df.meta.callsign.clone().unwrap_or_default(),
            },
        );
    }
    sink.emit(
        "session_decoded",
        SessionDecodedPayload {
            session_id: df.session_id,
            session_dir: df.session_dir.to_string_lossy().into_owned(),
            decoded_path: df.decoded_path.to_string_lossy().into_owned(),
            size: df.payload.len() as u32,
            filename: df.meta.filename.clone(),
            callsign: df.meta.callsign.clone(),
        },
    );

    let dir = save_dir.lock().ok().map(|p| p.clone()).unwrap_or_default();
    let env = PayloadEnvelope::decode_or_fallback(&df.payload);
    let (fname, callsign, content) = if env.version != 0 {
        (env.filename.clone(), env.callsign.clone(), env.content.clone())
    } else {
        (
            format!("decoded_{:08x}.bin", df.session_id),
            String::new(),
            df.payload.clone(),
        )
    };
    // If the payload is zstd-compressed (the "non-image file" case), we
    // decompress before writing. The envelope's filename is the original
    // one (without the `.zst` suffix), so we write it as-is.
    let (final_content, final_mime) = if df.meta.mime_type == modem_framing::app_header::mime::ZSTD {
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
        (content, df.meta.mime_type)
    };
    match save_file(&dir, &fname, &final_content) {
        Ok(path) => {
            sink.emit(
                "file_complete",
                FileCompletePayload {
                    filename: fname,
                    callsign,
                    mime_type: final_mime,
                    saved_path: path.to_string_lossy().into_owned(),
                    sigma2,
                    sigma2_data_avg,
                    size: final_content.len(),
                },
            );
        }
        Err(e) => {
            sink.emit(
                "error",
                ErrorPayload {
                    message: format!("save failed: {e}"),
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn profile_name(p: ProfileIndex) -> String {
    format!("{p:?}")
}

fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim();
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if cleaned.is_empty() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("received_{ts}.bin")
    } else {
        cleaned
    }
}

fn save_file(dir: &Path, filename: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let safe = sanitize_filename(filename);
    let path = unique_path(dir, &safe);
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Return a free path inside `dir`: if `filename` already exists, append
/// a `(1)`, `(2)`, ... suffix before the extension, up to 9999. Beyond
/// that (pathological case), fall back to a timestamp suffix to
/// guarantee uniqueness without ever overwriting an earlier reception.
fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let p = Path::new(filename);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(filename);
    let ext = p.extension().and_then(|s| s.to_str());
    for n in 1..=9999u32 {
        let alt = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(&alt);
        if !candidate.exists() {
            return candidate;
        }
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let alt = match ext {
        Some(e) => format!("{stem}_{ts}.{e}"),
        None => format!("{stem}_{ts}"),
    };
    dir.join(alt)
}

#[cfg(test)]
mod tests {
    use super::unique_path;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nbfm_unique_path_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn returns_filename_when_dir_empty() {
        let dir = tmp_dir("empty");
        let p = unique_path(&dir, "photo.jpg");
        assert_eq!(p, dir.join("photo.jpg"));
    }

    #[test]
    fn appends_suffix_when_file_exists() {
        let dir = tmp_dir("exists");
        fs::write(dir.join("photo.jpg"), b"a").unwrap();
        let p = unique_path(&dir, "photo.jpg");
        assert_eq!(p, dir.join("photo (1).jpg"));
    }

    #[test]
    fn increments_until_free() {
        let dir = tmp_dir("increments");
        fs::write(dir.join("photo.jpg"), b"a").unwrap();
        fs::write(dir.join("photo (1).jpg"), b"b").unwrap();
        fs::write(dir.join("photo (2).jpg"), b"c").unwrap();
        let p = unique_path(&dir, "photo.jpg");
        assert_eq!(p, dir.join("photo (3).jpg"));
    }

    #[test]
    fn handles_no_extension() {
        let dir = tmp_dir("noext");
        fs::write(dir.join("readme"), b"a").unwrap();
        let p = unique_path(&dir, "readme");
        assert_eq!(p, dir.join("readme (1)"));
    }

    #[test]
    fn deemphasis_dc_gain_is_unity() {
        let mut filter = super::DeemphasisFilter::new();
        let mut buf = vec![1.0f32; 4096];
        filter.process(&mut buf);
        // After enough samples to settle, DC output must equal DC input
        // (the IIR has unity DC gain by construction).
        let tail = &buf[buf.len() - 256..];
        let mean: f32 = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!(
            (mean - 1.0).abs() < 1e-3,
            "DC gain should be 1.0, got {mean}",
        );
    }

    #[test]
    fn deemphasis_nyquist_gain_is_minus_20_db() {
        // A square wave alternating ±1 every sample sits at Nyquist
        // (z = -1). Steady-state output amplitude should be the high-
        // shelf plateau gain = (b0 - b1) / (1 - a1) = 0.197 / 1.973 = 0.1.
        let mut filter = super::DeemphasisFilter::new();
        let mut buf: Vec<f32> = (0..8192)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        filter.process(&mut buf);
        let tail = &buf[buf.len() - 64..];
        let peak = tail.iter().cloned().fold(0.0f32, |m, x| m.max(x.abs()));
        // -20 dB = 0.1, accept 5% slack for discretization
        assert!(
            (peak - 0.1).abs() < 0.005,
            "Nyquist gain should be ~0.1 (-20 dB), got {peak}",
        );
    }

    #[test]
    fn deemphasis_passes_through_when_disabled_path() {
        // Sanity: with no filter applied (the worker stores `None`), the
        // batch is untouched. This mirrors how the run loop behaves when
        // `rx_deemphasis_enabled = false`.
        let original: Vec<f32> = (0..1024).map(|i| (i as f32) * 0.001).collect();
        let mut buf = original.clone();
        // No call to filter.process -> buffer unchanged.
        assert_eq!(buf, original);
    }
}
