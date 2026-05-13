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
use modem_core2x::frame2x::full_cycle_len_syms;
use modem_core2x::gate2x::{PreambleProbe2x, IDLE_PROBE_BUF_SAMPLES, PROBE_THRESHOLD_2X};
use modem_core2x::plheader::{sof_for_family, SOF_LEN_SYM};
use modem_core2x::profile2x::{config_by_name_2x, ModemConfig2x};
use modem_core2x::rx_v4::{self, RxResult2x};
use modem_framing::app_header::AppHeader;
use modem_framing::payload_envelope::PayloadEnvelope;
use modem_worker_base::{EventSink, EventSinkExt, SharedWavSink, WorkerHandle};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Cubic-Lagrange (Farrow-style) resampler at a fixed ratio
/// `1 + ppm × 1e-6`. Compensates the TX↔RX sample-rate drift the worker
/// estimates from multi-PLHEADER LS fit, before handing the corrected
/// audio to [`audio_to_symbols`].
///
/// On a 60-second buffer at 48 kHz, a 4-tap cubic interpolation per
/// output sample costs ≈ 30 ms on Pi 5 — negligible compared to the
/// matched filter that follows. Linear interpolation (V3's
/// `rx_v2::resample_audio`) added measurable noise on this profile;
/// the cubic kernel preserves the RRC pulse shape to within
/// discretisation noise.
///
/// `ppm > 0` means RX captured more samples than TX produced (RX clock
/// runs faster) so the output is a SHORTER stream that matches TX
/// timing.
fn resample_audio_cubic(samples: &[f32], ppm: f64) -> Vec<f32> {
    if ppm.abs() < 0.1 {
        return samples.to_vec();
    }
    let ratio = 1.0 + ppm * 1e-6;
    if samples.len() < 4 {
        return samples.to_vec();
    }
    let n_out = ((samples.len() - 3) as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let t = (i as f64) * ratio;
        let idx = t.floor() as usize;
        let mu = (t - idx as f64) as f32;
        if idx + 2 < samples.len() && idx >= 1 {
            let s0 = samples[idx - 1];
            let s1 = samples[idx];
            let s2 = samples[idx + 1];
            let s3 = samples[idx + 2];
            // Cubic Lagrange interpolation coefficients (Farrow kernel,
            // Pham-Reibman variant). Same shape as the Phase A
            // `modem-core-base::farrow` interpolator but applied to a
            // dense audio stream at a fixed ratio rather than at
            // per-symbol strobes.
            let c0 = s1;
            let c1 = 0.5 * (s2 - s0);
            let c2 = s0 - 2.5 * s1 + 2.0 * s2 - 0.5 * s3;
            let c3 = -0.5 * s0 + 1.5 * s1 - 1.5 * s2 + 0.5 * s3;
            let v = ((c3 * mu + c2) * mu + c1) * mu + c0;
            out.push(v);
        } else if idx < samples.len() {
            out.push(samples[idx]);
        } else {
            break;
        }
    }
    out
}

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
    // `k*sps + (taps.len()-1)/2 = k*sps + 6*sps`. On a clean WAV→WAV
    // pipeline the matched filter is shift-invariant so the strobe
    // positions in the MF output are multiples of `sps` and `phase = 0`
    // is correct by construction.
    //
    // Real OTA paths (SDRplay, sound-card with AGC) introduce a sub-
    // sample shift between the TX's symbol grid and the RX's audio
    // sample grid. Locking `phase = 0` then samples between symbols
    // and the SOF correlator inside `rx_v4_symbols` never finds a
    // peak. Run the SOF-anchored phase search and USE its result —
    // the cost is `sps` correlations across the full audio (≈ 0.3 s
    // for 112 s @ sps=32 on Pi 5) and it scales linearly with capture
    // length. Cheap relative to the rx_v4 pipeline that follows.
    //
    // Pre-`v0.11.0-2x4` the search result was assigned to `_phase`
    // and `phase = 0` was hardcoded. That broke OTA captures with a
    // long band-noise prefix before the actual burst because the
    // 1024-sym cap landed entirely in noise.
    let phase = best_symbol_phase_sof(&mf, sps, cfg);

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
    // No upper cap on the symbol-position scan: the caller may hand us
    // a long capture (e.g. 112 s SDRplay grab with a band-noise prefix
    // followed by a brief burst near the end). A pre-`v0.11.0-2x4`
    // 1024-symbol cap (~0.7 s @ sps=32) landed entirely in noise and
    // picked a noise-optimal phase. Cost is `sps × (n_syms − SOF_LEN)`
    // 64-tap complex inner products — ≈ 32 × 168 k × 64 = 344 M
    // mults for 112 s sps=32, ≈ 0.3 s on Pi 5. Cheap relative to the
    // rx_v4 pipeline that runs after.

    let mut best_phase = 0usize;
    let mut best_peak = 0.0_f64;
    for p in 0..sps {
        let n_syms = (mf.len() - p) / sps;
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
///
/// Includes a two-pass drift bootstrap: the first pass at 0 ppm
/// locates every PLHEADER SOF in the symbol stream;
/// [`rx_v4::estimate_drift_from_sof_positions`] LS-fits the TX↔RX
/// symbol-clock offset from those positions; if the estimate is ≥
/// 0.5 ppm we resample the audio via [`resample_audio_cubic`] and run
/// the chain a second time. Mirrors the live worker's drift bootstrap
/// so a captured WAV decodes identically to the live capture that
/// produced it.
pub fn rx_v4_audio(
    samples: &[f32],
    cfg: &ModemConfig2x,
) -> Result<Option<RxResult2x>, String> {
    let symbols = audio_to_symbols(samples, cfg)?;
    let cycle_period_sym = full_cycle_len_syms(cfg);
    let baseline = rx_v4::rx_v4_symbols(&symbols, cfg);
    // Drift LS fit uses ONLY CRC-validated SOF positions (PLS Golay +
    // CRC16 both passed) — no false positives from noise correlations.
    // Returned by `rx_v4_symbols` in `validated_sof_positions`.
    let sof_positions = baseline
        .as_ref()
        .map(|r| r.validated_sof_positions.clone())
        .unwrap_or_default();
    if std::env::var_os("RX_V4_LOG_DRIFT").is_some() {
        eprintln!(
            "[drift-cli] syms={} validated_sofs={} positions={:?} baseline_cycles={}",
            symbols.len(),
            sof_positions.len(),
            sof_positions.iter().take(8).collect::<Vec<_>>(),
            baseline.as_ref().map_or(0, |r| r.cycles),
        );
    }
    if let Some(ppm) =
        rx_v4::estimate_drift_from_sof_positions(&sof_positions, cycle_period_sym)
    {
        if ppm.abs() >= 0.5 {
            let corrected = resample_audio_cubic(samples, ppm);
            let new_symbols = audio_to_symbols(&corrected, cfg)?;
            let corrected_res = rx_v4::rx_v4_symbols(&new_symbols, cfg);
            // Pick whichever result has more converged data CWs.
            let score = |r: &Option<RxResult2x>| -> usize {
                r.as_ref().map(|x| x.data_cws_converged).unwrap_or(0)
            };
            if std::env::var_os("RX_V4_LOG_DRIFT").is_some() {
                eprintln!(
                    "[drift-cli] ppm={:+.1} baseline_cws={} corrected_cws={}",
                    ppm,
                    score(&baseline),
                    score(&corrected_res),
                );
            }
            return Ok(if score(&corrected_res) >= score(&baseline) {
                corrected_res
            } else {
                baseline
            });
        }
    }
    Ok(baseline)
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

/// Per-batch level metric the GUI's top-bar VU meter consumes. Same
/// shape and emit cadence as V3's `rx_worker::AudioLevelPayload` so the
/// `audio_level` listener in `main.js` drives both families without
/// branching.
#[derive(Debug, Clone, Serialize)]
struct AudioLevelPayload {
    rms: f32,
    peak: f32,
    total_samples: u64,
    overdrive: bool,
    crest_db: f32,
}

/// Real-time margin telemetry, emitted every ~2 s. Carries lag,
/// per-batch processing time and the cumulative cpal drop counter so
/// the GUI's `noteRxRealtime` chip can flag CPU overload. Same wire
/// contract as V3's `rx_worker::RxRealtimePayload`.
#[derive(Debug, Clone, Serialize)]
struct RxRealtimePayload {
    lag_ms: f64,
    last_batch_ms: f64,
    max_batch_ms: f64,
    session_buf_ms: f64,
    dropped_samples: u64,
}

/// V3-parity overdrive thresholds. RMS above gate AND crest factor
/// below `OVERDRIVE_CREST_GATE_DB` triggers a hard-clipped diagnostic
/// — mirrors `rx_worker.rs` so the same TX-overdrive UI badge lights
/// up on either family.
const OVERDRIVE_RMS_GATE_LINEAR: f32 = 0.056;
const OVERDRIVE_CREST_GATE_DB: f32 = 8.5;

/// RMS floor under which the worker treats the chunk as silence and
/// leaves the audio buffer trimmed to its idle window. Anything above
/// extends the `probe_hot_until` window so the decode pipeline runs.
/// Same value V3's `rx_worker` uses for its silence detector.
const SILENCE_RMS_THRESHOLD: f32 = 0.005;

/// Hard cap on `audio_buffer` size even while warm. On chains with
/// continuous in-band noise (SDRplay open-squelch FM) the RMS gate
/// stays hot forever; without a cap the buffer would grow unbounded
/// and `audio_to_symbols` cost would grow linearly with capture
/// duration, eventually blocking the decode thread.
///
/// 60 s × 48 kHz = 2.88 M samples covers up to a 50 kB HIGH+2X burst
/// (~50 s at HIGH+2X net rate). `audio_to_symbols` at this size on
/// Pi 5 ≈ 250-300 ms per scan — fine at the
/// [`MIN_SCAN_WALL_INTERVAL`] cadence (≤ 30 % of one core).
const WARM_MAX_BUF_SAMPLES: usize = 60 * AUDIO_RATE as usize;

/// Wall-clock floor between two scan invocations. Sustains decode
/// even when audio chunks accumulate faster than rx_v4_symbols can
/// complete, by skipping scans rather than queuing them.
///
/// Tuned around the worst-case `audio_to_symbols` + `rx_v4_symbols`
/// runtime on the warm-buffer cap (60 s × 48 kHz). Empirically that's
/// ≈ 300 ms on Pi 5; 500 ms throttle leaves ≥ 200 ms headroom for the
/// rest of the worker loop (event emit, audio_level, mpsc recv).
const MIN_SCAN_WALL_INTERVAL: Duration = Duration::from_millis(500);

/// Single-pass peak / RMS / crest factor. Identical implementation to
/// `rx_worker::compute_audio_stats` — kept local rather than re-
/// exported to keep modem-worker2x free of a hard dependency on V3.
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

/// Minimum samples in the audio buffer before the probe is willing to
/// run. The FFT zero-pads short buffers, so a partial fill still
/// produces a meaningful peak² / mean² ratio — the floor exists only
/// to keep us from probing a single dust mote of audio on every chunk.
///
/// 0.5 s @ 48 kHz = ~1.5 PLHEADER cycles' worth at the densest profile
/// (sps=32, full PLHEADER ≈ 0.13 s). Even a multi-burst capture with
/// 1.5 s of inter-burst silence has the next burst's PLHEADER landing
/// inside this much audio by the time the gate fires, so we don't lock
/// onto the EOT cycle by mistake.
const PROBE_MIN_BUF_SAMPLES: usize = 24_000;

/// Audio samples accumulated between two `audio_to_symbols` +
/// `rx_v4_symbols` invocations while the gate is hot. Audio-budget
/// instead of wall-clock so test paths that push chunks back-to-back
/// (no real-time pacing) get the same decode cadence as live captures.
/// 24 000 samples @ 48 kHz = 0.5 s — one chunk's worth at typical
/// cpal / SDR delivery cadence, ≈ V3's per-batch scan rhythm.
const SCAN_AUDIO_DELTA: usize = 24_000;

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
    dropped_samples: Arc<AtomicU64>,
) -> WorkerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        // One-line stderr ping so the operator can verify (via terminal
        // launch) that the worker is actually running V4 and not the
        // V3 path under a forced-profile typo. Mirrors V3's
        // `[worker] start V3 profile=…` log without touching the
        // worker_log file.
        eprintln!(
            "[worker] start NBFM-2x profile={profile_name}"
        );
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
        // Rolling audio buffer. Stays small (IDLE_PROBE_BUF_SAMPLES)
        // while no burst is detected; grows to cover the full burst once
        // the gate fires hot, then drops back on EOT finalise. We feed
        // it through `audio_to_symbols` + `rx_v4_symbols` instead of the
        // closed-loop `StreamingFrontend` (Farrow + Gardner) because the
        // open-loop, SOF-anchored phase pick is the only one robust
        // enough to acquire on real OTA captures today — the streaming
        // front-end's Gardner TED drifts on the band-noise prefix
        // before a burst and never re-acquires. Same pattern V3's
        // `rx_worker::WorkerState::session_buffer` uses, audio-domain
        // sliding window.
        let mut audio_buffer: Vec<f32> = Vec::with_capacity(48_000 * 8);
        let mut last_scan_audio_len: usize = 0;
        let mut last_scan_wall: Option<Instant> = None;
        // Cached TX↔RX symbol-rate drift in ppm. Bootstrapped on the
        // first scan that produces ≥ 2 SOF detections via
        // `find_all_sofs` + `estimate_drift_from_sof_positions`, then
        // applied as a Farrow cubic-Lagrange resample to subsequent
        // decode attempts. Per-CW phase residual (after this bulk
        // correction) is absorbed by the turbo Pass 2 EM RTS smoother
        // already inside `rx_v4_symbols`.
        let mut cached_drift_ppm: f64 = 0.0;
        // V3-parity telemetry. `total_samples` is the cumulative count
        // of f32 audio samples the worker has observed (drives the GUI
        // VU meter's frame counter). `last_telemetry_tick` paces the
        // 2 s rx_realtime emit; `*_batch_processing_ms` track the wall
        // time spent in each chunk so the chip can show the worst
        // recent batch.
        let started = Instant::now();
        let mut total_samples: u64 = 0;
        let mut last_telemetry_tick = Instant::now();
        let mut last_batch_processing_ms: f64 = 0.0;
        let mut max_batch_processing_ms: f64 = 0.0;
        // Diagnostic logging for OTA bring-up. Set `RX_V4_LOG_GATE=1`
        // before launching the GUI (or CLI) to dump one line per probe
        // call (audio RMS, peak² / mean² ratio, anchor, pass/fail) and
        // one line per `rx_v4_symbols` call (cycles found, SOF locked,
        // converged CW counts). Mute by default so production captures
        // don't spew. Snapshot once at thread start — env reads in a
        // tight loop add measurable latency on Pi 5.
        let log_gate = std::env::var_os("RX_V4_LOG_GATE").is_some();
        // `RX_V4_NO_GATE=1` bypasses the FFT probe entirely — every
        // tick (still throttled to SCAN_INTERVAL_MS) runs `audio_to_symbols`
        // and `rx_v4_symbols`. Useful when bringing up OTA on a new
        // radio and unsure whether the gate or the decoder is the
        // blocker.
        let no_gate = std::env::var_os("RX_V4_NO_GATE").is_some();
        // Dedup state: once we emit `session_armed` / `session_decoded`
        // / `file_complete` for a given session_id, we don't re-emit
        // them on a later tick that still sees the same session. The
        // app_header is captured the first time it converges; the
        // payload is finalised either on EOT or at channel close.
        let mut emitted_session_id: Option<u32> = None;
        let mut last_progress_converged: usize = 0;
        // Set when the current burst's `file_complete` has fired. Reset
        // back to `false` after the worker swallows the EOT and the
        // session state has been rolled over for a possible next burst
        // (multi-burst capture). The channel-close fallback path below
        // honours `finalised = true` to skip a redundant final pass when
        // the LAST burst ended cleanly on its EOT.
        let mut finalised = false;
        // True as soon as the worker has finalised at least one session
        // in this capture. Used to suppress spurious "no burst detected"
        // / "decode incomplete" errors at channel close in a multi-burst
        // capture: a burst that didn't make it through after a clean
        // one already landed is silence, not failure.
        let mut any_session_finalised = false;

        // Idle-gate state (Phase C-5).
        let mut last_probe_at: Option<Instant> = None;
        let mut probe_hot_until: Option<Instant> = None;
        loop {
            let chunk = match samples.recv() {
                Ok(c) => c,
                Err(_) => break, // sender dropped → exit and run a final tick.
            };
            let batch_start = Instant::now();
            total_samples += chunk.len() as u64;
            if let Ok(mut g) = wav_sink.lock() {
                if let Some(ws) = g.as_mut() {
                    ws.write_chunk(&chunk);
                }
            }
            // V3-parity VU meter feed. Emit on every chunk (~500 ms at
            // 48 kHz) so the GUI's top-bar level indicator stays alive
            // regardless of decode activity — without this the bar
            // freezes the moment the user picks a 2x profile because
            // no `audio_level` event ever reaches the frontend.
            let (peak, rms, crest_db) = compute_audio_stats(&chunk);
            let overdrive =
                rms > OVERDRIVE_RMS_GATE_LINEAR && crest_db < OVERDRIVE_CREST_GATE_DB;
            sink.emit(
                "audio_level",
                AudioLevelPayload {
                    rms,
                    peak,
                    total_samples,
                    overdrive,
                    crest_db,
                },
            );
            // Accumulate into the rolling audio buffer. Trim to
            // IDLE_PROBE_BUF_SAMPLES while we're cold (no recent audio
            // activity AND no active session) so memory stays bounded
            // during long quiet captures. Once a burst is in flight
            // (`emitted_session_id.is_some()` OR within
            // `PROBE_HOT_HOLD` of recent audio activity) the buffer
            // grows freely to cover the whole burst — `audio_to_symbols`
            // needs the full PLHEADER..EOT span to land an SOF
            // correlation peak.
            audio_buffer.extend_from_slice(&chunk);
            let now = Instant::now();

            // RMS-based activity detector. Simpler and more robust than
            // the FFT preamble probe: the FFT template is flat-spectrum
            // and the typical SDR + radio NBFM RX chain LP-filters the
            // audio at ~300 Hz (de-emphasis), squashing the template
            // overlap with the received signal to noise-level ratios
            // even on excellent-SNR captures. RMS over silence threshold
            // means "something's on air"; rx_v4_symbols's SOF
            // correlator (already at 0.2 × SOF_LEN threshold) is the
            // real structure detector that follows.
            //
            // V3 worker uses the same `SILENCE_RMS_THRESHOLD = 0.005`
            // for its silence detector — parity in spirit.
            if rms > SILENCE_RMS_THRESHOLD {
                probe_hot_until = Some(now + PROBE_HOT_HOLD);
            }
            if log_gate {
                eprintln!(
                    "[gate] buf={:.1}s rms={:.4} thr={:.4} hot={}",
                    audio_buffer.len() as f64 / AUDIO_RATE as f64,
                    rms,
                    SILENCE_RMS_THRESHOLD,
                    probe_hot_until.map_or(false, |t| now < t),
                );
            }

            let in_hot_window = probe_hot_until.map_or(false, |t| now < t);
            let is_active = emitted_session_id.is_some();
            let warm = in_hot_window || is_active || no_gate;
            // Trim to the idle window when cold (small).
            if !warm && audio_buffer.len() > IDLE_PROBE_BUF_SAMPLES {
                let drop_n = audio_buffer.len() - IDLE_PROBE_BUF_SAMPLES;
                audio_buffer.drain(..drop_n);
                last_scan_audio_len = last_scan_audio_len.saturating_sub(drop_n);
            }
            // Hard cap when warm. Without this, an always-hot capture
            // (continuous in-band noise on the SDRplay) grows the
            // buffer unbounded and audio_to_symbols saturates the
            // decode thread.
            if warm && audio_buffer.len() > WARM_MAX_BUF_SAMPLES {
                let drop_n = audio_buffer.len() - WARM_MAX_BUF_SAMPLES;
                audio_buffer.drain(..drop_n);
                last_scan_audio_len = last_scan_audio_len.saturating_sub(drop_n);
            }

            // Suppress the wall-clock probe state — replaced by the
            // RMS gate above. Variable kept for log line parity but no
            // longer drives any decision.
            let _ = last_probe_at;

            // Wall-clock floor in addition to the audio-delta budget:
            // if rx_v4_symbols takes longer than the chunk cadence, we
            // skip scans rather than queue them. Important on Pi 5
            // where a 15 s buffer's audio_to_symbols + decode can hit
            // 150-300 ms while chunks arrive every ~25 ms (SDRplay
            // backend).
            let scan_wall_ok = last_scan_wall
                .map_or(true, |t| now.duration_since(t) >= MIN_SCAN_WALL_INTERVAL);

            let should_decode = !finalised
                && warm
                && audio_buffer.len() >= AUDIO_RATE as usize / 2
                && audio_buffer.len() - last_scan_audio_len >= SCAN_AUDIO_DELTA
                && scan_wall_ok;

            if should_decode {
                last_scan_audio_len = audio_buffer.len();
                last_scan_wall = Some(now);

                // Drift compensation: resample the audio buffer at the
                // currently-cached ppm before handing to
                // `audio_to_symbols`. Cubic Lagrange (Farrow kernel)
                // preserves RRC pulse shape better than linear interp.
                // The first scan always runs at ppm=0 to bootstrap the
                // SOF locations needed for the LS fit below.
                let working_buf = if cached_drift_ppm.abs() >= 0.5 {
                    resample_audio_cubic(&audio_buffer, cached_drift_ppm)
                } else {
                    audio_buffer.clone()
                };
                let symbols_opt = audio_to_symbols(&working_buf, &cfg);
                let symbols = match symbols_opt {
                    Ok(s) => s,
                    Err(e) => {
                        if log_gate {
                            eprintln!("[rxv4] audio_to_symbols error: {e}");
                        }
                        Vec::new()
                    }
                };

                // Drift bootstrap / refinement. Find every SOF in the
                // current symbol stream; if we have ≥ 2, LS-fit the
                // ppm and update the cache. Re-decode at the new ppm
                // if the estimate moved meaningfully (≥ 5 ppm shift),
                // so a converged drift estimate doesn't have to wait
                // for the next chunk.
                let cycle_period_sym = full_cycle_len_syms(&cfg);
                let min_skip = (cycle_period_sym / 2).max(SOF_LEN_SYM);
                let sof_positions =
                    modem_core2x::rx_v4::find_all_sofs(&symbols, cfg.family, min_skip);
                let new_drift = modem_core2x::rx_v4::estimate_drift_from_sof_positions(
                    &sof_positions,
                    cycle_period_sym,
                );
                let mut symbols = symbols;
                if let Some(ppm) = new_drift {
                    let shift = (ppm - cached_drift_ppm).abs();
                    if shift >= 5.0 {
                        if log_gate {
                            eprintln!(
                                "[drift] sofs={} cached={:+.1} → new={:+.1} ppm (Δ={:.1}) — re-decoding",
                                sof_positions.len(),
                                cached_drift_ppm,
                                ppm,
                                shift,
                            );
                        }
                        cached_drift_ppm = ppm;
                        let rebuf = resample_audio_cubic(&audio_buffer, cached_drift_ppm);
                        if let Ok(new_syms) = audio_to_symbols(&rebuf, &cfg) {
                            symbols = new_syms;
                        }
                    } else if log_gate {
                        eprintln!(
                            "[drift] sofs={} cached={:+.1} ppm (stable, Δ={:.1})",
                            sof_positions.len(),
                            cached_drift_ppm,
                            shift,
                        );
                    }
                }

                let res_opt = if symbols.len() >= 192 {
                    modem_core2x::rx_v4::rx_v4_symbols(&symbols, &cfg)
                } else {
                    None
                };
                if log_gate {
                    match &res_opt {
                        Some(r) => eprintln!(
                            "[rxv4] audio={:.1}s syms={} cycles={} \
                             data_cws={}/{} sof@={:?} eot={} ppm={:+.1}",
                            audio_buffer.len() as f64 / AUDIO_RATE as f64,
                            symbols.len(),
                            r.cycles,
                            r.data_cws_converged,
                            r.data_cws_total,
                            r.first_sof_at,
                            r.eot_seen,
                            cached_drift_ppm,
                        ),
                        None => eprintln!(
                            "[rxv4] audio={:.1}s syms={} → no SOF (ppm={:+.1})",
                            audio_buffer.len() as f64 / AUDIO_RATE as f64,
                            symbols.len(),
                            cached_drift_ppm,
                        ),
                    }
                }
                if let Some(res) = res_opt {
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
                            // V3's progress bar denominator is DATA-only
                            // (META is framing overhead, not part of the
                            // RaptorQ ESI space). V4 mirrors that with
                            // `data_cws_total / data_cws_converged` so the
                            // fountain-fill math and the `converged_bitmap`
                            // hole map line up bit-for-bit with V3.
                            sink.emit(
                                "v2_progress",
                                serde_json::json!({
                                    "blocks_converged": res.data_cws_converged,
                                    "blocks_total": res.data_cws_total,
                                    "blocks_expected": res.data_cws_total,
                                    "converged_bitmap":
                                        res.converged_bitmap.clone(),
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
                        // close. Then roll the worker's session-decode
                        // state back to "idle, looking for next burst"
                        // so a subsequent TX on the same capture
                        // produces another session_armed / file_complete
                        // pair instead of being silently dropped.
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
                            any_session_finalised = true;
                            // Multi-burst reset: drop the audio buffer
                            // and the gate state so the next burst
                            // looks like a fresh capture. No timing
                            // loop to rewind here — the audio-domain
                            // SOF-anchored phase pick is stateless per
                            // call.
                            emitted_session_id = None;
                            last_progress_converged = 0;
                            finalised = false;
                            audio_buffer.clear();
                            audio_buffer.shrink_to_fit();
                            last_scan_audio_len = 0;
                            last_scan_wall = None;
                            last_probe_at = None;
                            probe_hot_until = None;
                            // Keep cached_drift_ppm: same TX→RX chain
                            // between bursts, so the next burst should
                            // share the same clock offset. Saves one
                            // bootstrap iteration per burst.
                        }
                    }
                }
            }
            // V3-parity rx_realtime telemetry. Track per-batch wall
            // time and emit a digest every ~2 s so the GUI's CPU-margin
            // chip has fresh state. dropped_samples comes from the cpal
            // mpsc backpressure counter (still 0 on SDR backends, which
            // pre-buffer in-thread).
            last_batch_processing_ms = batch_start.elapsed().as_secs_f64() * 1000.0;
            if last_batch_processing_ms > max_batch_processing_ms {
                max_batch_processing_ms = last_batch_processing_ms;
            }
            if last_telemetry_tick.elapsed() >= Duration::from_secs(2) {
                let wall_s = started.elapsed().as_secs_f64();
                let audio_s = total_samples as f64 / AUDIO_RATE as f64;
                let lag_ms = (wall_s - audio_s) * 1000.0;
                let session_buf_ms =
                    audio_buffer.len() as f64 * 1000.0 / AUDIO_RATE as f64;
                let dropped = dropped_samples.load(Ordering::Relaxed);
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
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }
        }

        // Channel-close or stop: one final pass to catch a burst that
        // ended without an EOT (truncated WAV, stop button mid-frame,
        // ...). If the LAST burst already EOT-finalised, skip — there
        // are no leftover symbols worth scanning. A multi-burst capture
        // that finalised an earlier burst on its EOT and saw no later
        // burst at all also falls through here; the `any_session_finalised`
        // flag suppresses the "no burst detected" / "decode incomplete"
        // errors below, which would otherwise contradict the earlier
        // successful `file_complete`.
        if finalised {
            return;
        }
        let symbols = audio_to_symbols(&audio_buffer, &cfg).unwrap_or_default();
        let result = if symbols.len() >= 192 {
            modem_core2x::rx_v4::rx_v4_symbols(&symbols, &cfg)
        } else {
            None
        };
        let Some(rx_result) = result else {
            // Only complain if we never saw a SOF AND never finalised a
            // prior burst. Otherwise emitted_id is Some (active session
            // in progress, GUI already shows it) or any_session_finalised
            // is true (multi-burst case: earlier burst landed cleanly,
            // trailing silence is normal) — silence is the right UX.
            if emitted_session_id.is_none() && !any_session_finalised {
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
            if !any_session_finalised {
                sink.emit(
                    "error",
                    ErrorPayload {
                        message: format!(
                            "RX V4 : décodage incomplet ({}/{} CW)",
                            rx_result.converged_cws, rx_result.total_cws
                        ),
                    },
                );
            }
            return;
        };
        if rx_result.data.is_empty() {
            if !any_session_finalised {
                sink.emit(
                    "error",
                    ErrorPayload {
                        message: format!(
                            "RX V4 : payload vide (cycles={}, CW {}/{})",
                            rx_result.cycles, rx_result.converged_cws, rx_result.total_cws
                        ),
                    },
                );
            }
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
            // Sleep ≥ MIN_SCAN_WALL_INTERVAL between chunks so the
            // worker's wall-clock scan throttle (1 s) sees real wall
            // time pass and runs `rx_v4_symbols` at the expected
            // cadence. Without this, paced tests push chunks in
            // microseconds, the wall throttle clamps to one scan per
            // run, and multi-burst captures never make it past the
            // first session.
            std::thread::sleep(std::time::Duration::from_millis(100));
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
    fn spawn_decodes_two_consecutive_bursts_in_one_capture() {
        // Multi-burst contract: two payloads encoded back-to-back with
        // INTER_BURST_SILENCE_S of zeros between them. The worker should
        // emit two `file_complete` events with distinct session_id /
        // filenames and write both decoded files. Regression for the
        // old single-shot behaviour where `finalised = true` froze the
        // worker after burst 1 and burst 2 was silently dropped.
        use modem_framing::payload_envelope::PayloadEnvelope;
        use modem_worker_base::RecordingSink;
        use std::sync::mpsc;

        let content_a = rng_bytes(200, 0x11);
        let content_b = rng_bytes(220, 0x22);
        let env_a =
            PayloadEnvelope::new("burst_a.bin", "HB9TOB", content_a.clone()).expect("env A");
        let env_b =
            PayloadEnvelope::new("burst_b.bin", "HB9TOB", content_b.clone()).expect("env B");
        let cfg = profile_high_2x();
        let k_bytes = cfg.base.ldpc_rate.k() / 8;

        let mut audio: Vec<f32> = Vec::new();
        for (env, sid) in [(&env_a, 0xAA00_0001_u32), (&env_b, 0xBB00_0002_u32)] {
            let wire = env.encode();
            let k_source =
                modem_framing::raptorq_codec::k_from_payload(wire.len(), k_bytes) as u32;
            let n_packets =
                k_source + modem_framing::raptorq_codec::n_repair_default(k_source);
            let req = EncodeRequest {
                profile: "HIGH2X",
                wire_payload: &wire,
                session_id: sid,
                mime_type: mime::BINARY,
                hash_short: 0x1234,
                esi_start: 0,
                n_packets,
                vox_seconds: 0.0,
            };
            audio.extend_from_slice(
                &V4Modem.encode_to_samples(&req).expect("encode"),
            );
            // Inter-burst silence — ≥ a few PROBE_INTERVALs so the gate
            // closes between bursts. 1.5 s is plenty without bloating
            // the test runtime.
            const INTER_BURST_SILENCE_S: f64 = 1.5;
            let n_silence =
                (INTER_BURST_SILENCE_S * AUDIO_RATE as f64).round() as usize;
            audio.extend(std::iter::repeat(0.0_f32).take(n_silence));
        }

        let tmp = tempfile::tempdir().expect("tmp");
        let save_dir = Arc::new(Mutex::new(tmp.path().to_path_buf()));
        let wav_sink: SharedWavSink = Arc::new(Mutex::new(None));
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
        const BATCH: usize = 24_000;
        for i in (0..audio.len()).step_by(BATCH) {
            let end = (i + BATCH).min(audio.len());
            tx.send(audio[i..end].to_vec()).expect("send");
            // Sleep ≥ MIN_SCAN_WALL_INTERVAL between chunks so the
            // worker's wall-clock scan throttle (1 s) sees real wall
            // time pass and runs `rx_v4_symbols` at the expected
            // cadence. Without this, paced tests push chunks in
            // microseconds, the wall throttle clamps to one scan per
            // run, and multi-burst captures never make it past the
            // first session.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        drop(tx);
        handle
            .thread
            .take()
            .expect("worker thread present")
            .join()
            .expect("worker join");

        let events = recording.events();
        let file_completes: Vec<_> = events
            .iter()
            .filter(|(n, _)| n == "file_complete")
            .map(|(_, v)| v.clone())
            .collect();
        let err_msg = events
            .iter()
            .find(|(n, _)| n == "error")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        let event_names: Vec<_> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            file_completes.len(),
            2,
            "expected 2 file_complete events, got {}: error={err_msg} events={event_names:?}",
            file_completes.len(),
        );
        // file_complete[i].filename must match what each envelope set.
        let names: Vec<&str> = file_completes
            .iter()
            .map(|fc| fc["filename"].as_str().expect("filename"))
            .collect();
        assert!(names.contains(&"burst_a.bin"), "missing burst_a: {names:?}");
        assert!(names.contains(&"burst_b.bin"), "missing burst_b: {names:?}");
        // Both decoded files exist on disk and match the originals.
        for (fc, expected) in file_completes
            .iter()
            .zip([&content_a, &content_b].iter())
        {
            let path = fc["saved_path"].as_str().expect("saved_path");
            let written = std::fs::read(path).expect("read saved");
            // Same-named files in the same save_dir would clobber each
            // other; the envelopes use distinct filenames so they don't.
            // Match either order — file_completes come in arrival order.
            let matches_a = written == content_a;
            let matches_b = written == content_b;
            assert!(matches_a || matches_b, "decoded file ≠ either original");
            let _ = expected;
        }
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
