//! Family-independent TX runtime — playback orchestration, PTT
//! coordination, on-disk WAV archiving, and the event-payload structs
//! the GUI listens to.
//!
//! What this module owns:
//!
//! - The four TX event payloads (`TxPlanEvent`, `TxProgressEvent`,
//!   `TxCompleteEvent`, `TxErrorEvent`).
//! - `TxHandle` — Arc<AtomicBool> + JoinHandle pair returned by every
//!   `spawn` routine, mirrors `WorkerHandle` semantics one-to-one.
//! - [`run_playback`] — the core PTT-engage → SampleSink → progress
//!   polling → PTT-release loop. Takes a pre-rendered `Vec<f32>` (the
//!   V3 vs V4 split happens upstream, in the caller).
//! - [`archive_payload`], [`build_tx_wav_path`], [`write_tx_wav`],
//!   [`sanitize_filename`] — file-system glue around the TX history.
//!
//! What this module deliberately does NOT touch:
//!
//! - Modem encoding (`V3Modem.encode_to_samples` or
//!   `V4Modem.encode_to_samples`) — that's family-specific and stays in
//!   `modem-worker::tx_worker` / `modem-worker2x::tx_worker2x`. Those
//!   wrappers compute the audio buffer + plan, then hand off to this
//!   module's `run_playback`.
//! - Profile-arithmetic (`tx_plan`) — also family-specific.
//!
//! Dependency consequence: `modem-worker-base` gains direct deps on
//! `modem-io` (SampleSink trait), `modem-sdr-dsp` (NBFM pre-emphasis),
//! and `modem-framing` (mime byte enum for the archive metadata). All
//! three are family-independent transport / DSP layers, so this stays
//! consistent with the "shared infrastructure" charter the rest of the
//! crate follows.

use hound::{SampleFormat, WavSpec, WavWriter};
use modem_core_base::types::AUDIO_RATE;
use modem_io::SampleSink;
use modem_sdr_dsp::emphasis::preemphasis_nbfm_48k;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::event_sink::{EventSink, EventSinkExt};
use crate::ptt::{SharedPtt, PTT_GUARD_MS};

/// VOX preamble duration handed to the modem's `encode_to_samples`.
/// 0.5 s matches what `nbfm-modem tx` writes by default (`--vox 0.5`)
/// and what the GUI played on air before the in-process migration.
/// Required for stations whose transceiver triggers TX via VOX rather
/// than a wired PTT. Same constant for both families.
pub const TX_VOX_SECONDS: f64 = 0.5;

#[derive(Serialize, Clone)]
pub struct TxPlanEvent {
    pub duration_s: f64,
    pub total_blocks: u32,
    pub wire_bytes: u32,
    pub wav_path: String,
    pub mode: String,
    pub callsign: String,
    pub filename: String,
}

#[derive(Serialize, Clone)]
pub struct TxProgressEvent {
    pub pos_samples: u64,
    pub total_samples: u64,
    pub elapsed_s: f64,
    pub duration_s: f64,
    pub blocks_sent: u32,
    pub total_blocks: u32,
}

#[derive(Serialize, Clone)]
pub struct TxCompleteEvent {
    pub duration_s: f64,
    pub wav_path: String,
    pub stopped_early: bool,
}

#[derive(Serialize, Clone)]
pub struct TxErrorEvent {
    pub message: String,
}

/// Per-profile transmission plan, derived purely from arithmetic (no
/// symbol synthesis). Computed by both V3 (`modem-worker::tx_worker::tx_plan`)
/// and V4 (`modem-worker2x::tx_worker2x::tx_plan`) ahead of any TX so
/// the GUI can pre-populate the duration / block-count widgets and
/// expose the RaptorQ fountain math to the operator. The struct is
/// family-independent; only the routine that *computes* it knows about
/// frame layout.
pub struct TxPlan {
    /// Number of LDPC codewords required at RX to reconstruct the payload
    /// (the RaptorQ "K" source-symbol count). The RX must accumulate at
    /// least this many unique ESIs before `try_decode` succeeds.
    pub k_source: u32,
    /// Number of codewords the initial TX burst actually emits (K + default
    /// repair ≈ 30 %). The progress bar counts up to here.
    pub n_initial: u32,
    /// Seconds of audio needed to transmit `n_initial` codewords at this
    /// profile's net bitrate. Includes pilot + LDPC overhead.
    pub duration_s_initial: f64,
    /// Seconds of audio needed just for `k_source` codewords — the minimum
    /// theoretical time before RX could decode if no packet was ever lost.
    pub duration_s_k: f64,
    /// Seconds per one additional codeword at this profile — used by the UI
    /// to convert "+N% More" to a duration estimate.
    pub seconds_per_cw: f64,
}

/// Per-TX cancellation token + join handle. Identical shape to
/// [`WorkerHandle`](crate::worker_handle::WorkerHandle) but distinct so
/// callers can hold an RX worker and a TX worker simultaneously without
/// type confusion (the GUI's `AppState` already does).
pub struct TxHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl TxHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Build the path the TX worker writes the synthesised WAV to when the
/// user enables the `tx_save_wav` toggle. Lives under `tx_history/` so
/// it sits next to the archived source file produced by
/// [`archive_payload`]. Includes a UNIX timestamp prefix to disambiguate
/// repeat TX of the same file, plus an `-r{esi}` tag for "More" bursts
/// so they don't overwrite the initial burst's WAV.
pub fn build_tx_wav_path(save_dir: &Path, filename: &str, esi_start: u32) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let history_dir = save_dir.join("tx_history");
    let _ = std::fs::create_dir_all(&history_dir);
    let safe = sanitize_filename(filename);
    let suffix = if esi_start == 0 {
        String::new()
    } else {
        format!("-r{esi_start}")
    };
    history_dir
        .join(format!("tx-{ts}-{safe}{suffix}.wav"))
        .to_string_lossy()
        .into_owned()
}

/// Write a mono int16 48 kHz WAV at `path` from a slice of f32 samples
/// in [-1.0, 1.0]. Same format the legacy `nbfm-modem tx` produced.
/// Errors are logged via stderr but never propagated — saving the WAV
/// is a best-effort side-channel, never a reason to abort the TX.
pub fn write_tx_wav(path: &str, samples: &[f32]) {
    let spec = WavSpec {
        channels: 1,
        sample_rate: AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = match WavWriter::create(path, spec) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[tx_save_wav] create {path}: {e}");
            return;
        }
    };
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        if let Err(e) = writer.write_sample(v) {
            eprintln!("[tx_save_wav] write {path}: {e}");
            return;
        }
    }
    if let Err(e) = writer.finalize() {
        eprintln!("[tx_save_wav] finalize {path}: {e}");
    }
}

/// Switch the PTT to TX polarity. Best-effort: if writing to the port
/// fails (cable unplugged mid-session, ...) we log and keep going — the
/// worker is not in the business of aborting a transmission for that.
fn ptt_engage(ptt: &SharedPtt) -> bool {
    let mut g = match ptt.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let Some(ctrl) = g.as_mut() else { return false };
    match ctrl.set_tx() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[ptt] set_tx: {e}");
            false
        }
    }
}

fn ptt_release(ptt: &SharedPtt) {
    let mut g = match ptt.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(ctrl) = g.as_mut() {
        if let Err(e) = ctrl.set_rx() {
            eprintln!("[ptt] set_rx: {e}");
        }
    }
}

/// Render the pre-built audio buffer to the chosen `SampleSink`,
/// emitting `tx_progress` ticks every ~200 ms and observing the `stop`
/// cancellation flag. Applies optional NBFM pre-emphasis (75 µs shelf)
/// and the dB-domain attenuation cascade BEFORE handing the buffer to
/// the sink; the post-processing buffer is also what gets dumped to
/// disk when `wav_path` is non-empty, so Audacity reproduces on-air
/// audio exactly.
///
/// PTT discipline: switch to TX, wait `PTT_GUARD_MS`, play, wait
/// `PTT_GUARD_MS` after playback completes, switch back to RX. A failed
/// `SampleSink::play_buffer` flashes the PTT briefly before reporting
/// `tx_error`; accepted trade-off for the simpler one-call sink trait.
pub fn run_playback(
    device_name: &str,
    mut samples: Vec<f32>,
    total_blocks: u32,
    duration_s: f64,
    wav_path: String,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    tx_sink: Arc<dyn SampleSink>,
    stop: Arc<AtomicBool>,
    ptt: SharedPtt,
    sink: Arc<dyn EventSink>,
) {
    // Optional NBFM pre-emphasis (+6 dB/oct, tau = 750 us). Applied
    // BEFORE the attenuation so that the re-normalized peak still
    // respects the ATT setpoint. The shelf strongly lifts the high audio
    // frequencies (+13 dB at 1 kHz, +18 dB at 2.7 kHz, plateau +20 dB):
    // without re-normalization the signal would clip the sound card.
    if preemphasis_enabled {
        preemphasis_nbfm_48k(&mut samples);
        // Re-peak-normalize back to the modem-cli output level
        // (PEAK_NORMALIZE = 0.9 in modem-core::types). Keeps ~0.9 dB of
        // headroom before int16/F32 saturation regardless of the shelf
        // HF lift.
        let peak = samples.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        if peak > 0.9 {
            let scale = 0.9 / peak;
            for s in samples.iter_mut() {
                *s *= scale;
            }
        }
    }
    // Apply the ATT cascade attenuation (Channel tab). Clamp to
    // [-60, 0] dB defensively — beyond that range it serves no purpose
    // and an unexpected positive sign would saturate the sound card.
    let att_db = attenuation_db.clamp(-60.0, 0.0);
    if att_db < 0.0 {
        let gain = 10f32.powf(att_db / 20.0);
        for s in samples.iter_mut() {
            *s *= gain;
        }
    }
    // Optional WAV dump (settings: tx_save_wav). We write the
    // *post-processing* signal — what actually goes to the sound card /
    // Pluto — so reopening the file in Audacity reproduces the on-air
    // audio exactly. Best-effort: errors get logged, never abort the TX.
    if !wav_path.is_empty() {
        write_tx_wav(&wav_path, &samples);
    }
    let total_samples = samples.len();

    // PTT: switch to TX BEFORE opening the audio stream, then wait
    // 200 ms for the transceiver to commute. The single-step
    // SampleSink::play_buffer combines lookup+build+play, so a missing
    // device now flashes the PTT briefly before tx_error — accepted
    // trade-off for a simpler trait surface.
    let ptt_engaged = ptt_engage(&ptt);
    if ptt_engaged {
        thread::sleep(Duration::from_millis(PTT_GUARD_MS));
    }

    // The Tauri layer resolved `tx_sink` upfront (Pluto SampleSink for a
    // composite SDR device name, or CpalSink for a plain cpal name).
    // Both implementations return `modem_io::PlaybackHandle`, so the
    // polling loop below is uniform — one trait dispatch on
    // `play_buffer`, then direct method calls on the handle. The
    // `device_name` arg is ignored by the Pluto adapter (it already
    // knows its libiio URI from the SdrConfig that built the sink) but
    // kept for the cpal sink which still needs it for device lookup.
    let handle = match tx_sink.play_buffer(device_name, AUDIO_RATE, samples) {
        Ok(h) => h,
        Err(e) => {
            if ptt_engaged {
                ptt_release(&ptt);
            }
            sink.emit(
                "tx_error",
                TxErrorEvent {
                    message: e.to_string(),
                },
            );
            return;
        }
    };

    let start = Instant::now();
    let mut last_tick = Instant::now() - Duration::from_millis(300);
    let mut stopped_early = false;
    loop {
        thread::sleep(Duration::from_millis(100));
        let p = handle.pos();
        let done = handle.is_done();
        if stop.load(Ordering::Relaxed) {
            stopped_early = true;
        }
        let now = Instant::now();
        let should_emit =
            now.duration_since(last_tick) >= Duration::from_millis(200) || done || stopped_early;
        if should_emit {
            let elapsed_s = start.elapsed().as_secs_f64();
            let frac = if total_samples > 0 {
                p as f64 / total_samples as f64
            } else {
                1.0
            };
            let blocks_sent = ((frac * total_blocks as f64).round() as u32).min(total_blocks);
            sink.emit(
                "tx_progress",
                TxProgressEvent {
                    pos_samples: p as u64,
                    total_samples: total_samples as u64,
                    elapsed_s,
                    duration_s,
                    blocks_sent,
                    total_blocks,
                },
            );
            last_tick = now;
        }
        if done || stopped_early {
            break;
        }
    }

    drop(handle);
    // 200 ms of silence before releasing the PTT, then switch back to RX.
    if ptt_engaged {
        thread::sleep(Duration::from_millis(PTT_GUARD_MS));
        ptt_release(&ptt);
    }
    sink.emit(
        "tx_complete",
        TxCompleteEvent {
            duration_s: start.elapsed().as_secs_f64(),
            wav_path,
            stopped_early,
        },
    );
}

/// Archive the TX payload file under `<save_dir>/tx_history/` at the
/// moment the user starts a transmission. Guarantees that the TX
/// history traces every attempt, including the ones aborted mid-burst
/// (PTT released, audio error). Emits `tx_archived` on the frontend
/// and purges the oldest entries if `max_items` is exceeded.
///
/// `payload_path` must point at an existing file (`tx_preview.avif` or
/// `tx_preview.zst`). `filename` is the original name chosen by the
/// user, preserved as-is in the metadata for thumbnail display.
///
/// Family-agnostic: `mode` is stored as an opaque string, the mime
/// type is inferred from the file extension. Same routine for V3 and
/// V4 callers.
pub fn archive_payload(
    save_dir: &Path,
    payload_path: &Path,
    mode: &str,
    filename: &str,
    repair_pct: u32,
    max_items: u32,
    sink: &dyn EventSink,
) {
    let history_dir = save_dir.join("tx_history");
    if let Err(e) = std::fs::create_dir_all(&history_dir) {
        eprintln!("[tx_history] mkdir {:?}: {e}", history_dir);
        return;
    }
    let ext = payload_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("bin");
    let mime_type: u8 = match ext {
        "avif" => modem_framing::app_header::mime::IMAGE_AVIF,
        "zst" => modem_framing::app_header::mime::ZSTD,
        _ => modem_framing::app_header::mime::BINARY,
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let safe = sanitize_filename(filename);
    let stem = format!("{ts}-{safe}");
    let archive_path = history_dir.join(format!("{stem}.{ext}"));
    let meta_path = history_dir.join(format!("{stem}.json"));

    if let Err(e) = std::fs::copy(payload_path, &archive_path) {
        eprintln!("[tx_history] copy {:?}: {e}", payload_path);
        return;
    }
    let meta = serde_json::json!({
        "timestamp": ts,
        "mode": mode,
        "mime_type": mime_type,
        "filename": filename,
        "repair_pct": repair_pct,
    });
    if let Err(e) = std::fs::write(&meta_path, meta.to_string()) {
        eprintln!("[tx_history] write meta {:?}: {e}", meta_path);
    }
    purge_history(&history_dir, max_items);
    sink.emit("tx_archived", ());
}

/// Cap the `tx_history/` folder to `max_items` file+json pairs. Sort
/// the `.json` files by descending mtime and remove the oldest ones
/// (along with their twin source file).
fn purge_history(history_dir: &Path, max_items: u32) {
    let max = max_items.max(1) as usize;
    let mut metas: Vec<(SystemTime, PathBuf)> = match std::fs::read_dir(history_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
            })
            .filter_map(|e| {
                let m = e.metadata().ok()?.modified().ok()?;
                Some((m, e.path()))
            })
            .collect(),
        Err(_) => return,
    };
    if metas.len() <= max {
        return;
    }
    metas.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    for (_, json_path) in metas.into_iter().skip(max) {
        if let Some(stem) = json_path.file_stem().and_then(|s| s.to_str()) {
            if let Some(parent) = json_path.parent() {
                for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
                    let p = entry.path();
                    if p.file_stem().and_then(|s| s.to_str()) == Some(stem) {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }
}

/// Sanitize a filename for the filesystem: replace Windows/Linux
/// reserved characters with `_`, and truncate to 80 characters to
/// leave room for the timestamp prefix + extension.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        return "fichier".to_string();
    }
    if trimmed.len() > 80 {
        trimmed.chars().take(80).collect()
    } else {
        trimmed
    }
}
