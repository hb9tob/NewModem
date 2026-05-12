//! TX worker: synthesises the audio in-process via `V3Modem.encode_to_samples`
//! and plays it back on the selected sound card. Phase 2D-ii dropped the
//! legacy subprocess CLI pipeline (was: `nbfm-modem tx --input <avif>
//! --output <wav>`, then read back the WAV).
//!
//! Pipeline (dedicated thread, non-blocking for Tauri):
//!   1. Read the source bytes (AVIF or zstd, produced by tx_encode).
//!   2. Wrap in PayloadEnvelope (filename + callsign + content).
//!   3. Plan RaptorQ: K + repair → n_total packets.
//!   4. Build EncodeRequest, call V3Modem.encode_to_samples(req).
//!      VOX preamble stays at 0.5 s (radios without PTT need it —
//!      memory feedback_keep_vox).
//!   5. Play the resulting f32 samples via cpal on the chosen TX device.
//!
//! Events (sink.emit):
//!   - tx_plan      { duration_s, total_blocks, wire_bytes, wav_path,
//!                    mode, callsign, filename }
//!     `wav_path` is now empty (no intermediate file).
//!   - tx_progress  { pos_samples, total_samples, elapsed_s, duration_s,
//!                    blocks_sent (linearly interpolated), total_blocks }
//!   - tx_complete  { duration_s, wav_path, stopped_early }
//!   - tx_error     { message }

use modem_core::{
    profile::{self, ModemConfig, ProfileIndex},
    traits::{EncodeRequest, Modem},
    types::AUDIO_RATE,
    v3_modem::V3Modem,
};
use modem_framing::{
    app_header::{self, mime},
    payload_envelope::PayloadEnvelope,
    raptorq_codec::k_from_payload,
};
use modem_io::SampleSink;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;

use crate::event_sink::{EventSink, EventSinkExt};

// Family-independent TX infrastructure (event payloads, TxHandle,
// run_playback orchestration, on-disk archive helpers) lives in
// modem-worker-base. Re-exported here so existing GUI / CLI call sites
// (`use modem_worker::tx_worker::{TxHandle, TxPlanEvent, ...}`) keep
// compiling unchanged, and the V3 spawn/spawn_more wrappers below stay
// thin.
pub use modem_worker_base::tx_runtime::{
    archive_payload, build_tx_wav_path, run_playback, TxCompleteEvent, TxErrorEvent, TxHandle,
    TxPlan, TxPlanEvent, TxProgressEvent, TX_VOX_SECONDS,
};

use crate::ptt::SharedPtt;

pub fn parse_profile(name: &str) -> Result<ModemConfig, String> {
    profile::config_by_name(name).ok_or_else(|| format!("unknown profile '{name}'"))
}

// `TxPlan` lives in modem-worker-base::tx_runtime so the V4 sibling
// (`modem-worker2x::tx_worker2x::tx_plan`) returns the same struct
// without re-deriving it. Re-exported above.

/// Compute the transmission plan for a given payload + profile + chosen
/// RaptorQ repair percentage. Includes every frame overhead (preamble,
/// header, meta, markers, pilots, runout, EOT, plus periodic re-insertions
/// every `V3_PREAMBLE_PERIOD_S`) so the duration shown in the UI matches
/// what actually goes on air. Exact values come from
/// `frame::superframe_total_symbols` + `frame::eot_frame_symbols`, which
/// mirror the frame builder one-to-one.
pub fn tx_plan(
    payload_bytes: usize,
    mode_name: &str,
    callsign_len: usize,
    filename_len: usize,
    repair_pct: u32,
) -> Result<TxPlan, String> {
    let config = parse_profile(mode_name)?;
    let envelope_overhead = 2 + filename_len + 1 + callsign_len + 1;
    let wire = payload_bytes + envelope_overhead;
    let k_bytes = config.ldpc_rate.k() / 8;
    let k_source = k_from_payload(wire, k_bytes) as u32;
    // Align on what build_superframe_v3_range will actually emit: if the
    // raw n_initial is odd, the builder rounds it so the last data
    // segment is complete (otherwise RX loses that final CW).
    let n_initial =
        modem_core::frame::effective_packet_count(k_source + (k_source * repair_pct) / 100);
    // Asymptote payload-only : utile pour l'estimation marginale "+N blocs".
    let bits_per_cw = (k_bytes as f64) * 8.0;
    let seconds_per_cw = bits_per_cw / config.net_bitrate();

    // Actual duration = total superframe symbols + EOT symbols, divided
    // by the symbol rate. Captures periodic re-insertions (very costly on
    // ULTRA where there is one per segment) and every structural overhead.
    let total_syms_initial =
        modem_core::frame::superframe_total_symbols(&config, n_initial)
            + modem_core::frame::eot_frame_symbols(&config);
    let duration_s_initial = total_syms_initial as f64 / config.symbol_rate;
    // For duration_s_k: duration up to the K-th codeword, without EOT
    // (the RX can decode as soon as K are received, before the EOT).
    let duration_s_k =
        modem_core::frame::superframe_total_symbols(&config, k_source) as f64
            / config.symbol_rate;
    Ok(TxPlan {
        k_source,
        n_initial,
        duration_s_initial,
        duration_s_k,
        seconds_per_cw,
    })
}


/// FNV-1a 32-bit + xor-fold to 16 bits — same routine the legacy CLI
/// uses to derive the app-header `hash_short` field. Bit-for-bit
/// equivalent to `content_hash_short` in modem-cli/src/main.rs.
fn fnv_short(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

/// Map a source-file extension to the protocol-level mime byte. Mirrors
/// `infer_mime` in modem-cli/src/main.rs — the GUI sends either
/// `tx-preview.avif` (image) or `tx-preview.zst` (file mode).
fn infer_mime(path: &Path) -> u8 {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("avif") => mime::IMAGE_AVIF,
        Some("jpg") | Some("jpeg") => mime::IMAGE_JPEG,
        Some("png") => mime::IMAGE_PNG,
        Some("txt") | Some("md") => mime::TEXT,
        Some("zst") => mime::ZSTD,
        _ => mime::BINARY,
    }
}

/// Resolve a profile name to its protocol-level `ProfileIndex` byte
/// (used to seed `compute_session_id`). Returns `None` for an unknown
/// name; callers report it as a tx_error.
fn profile_index_for(name: &str) -> Option<u8> {
    ProfileIndex::from_name(name).map(ProfileIndex::as_u8)
}

/// Burst variant : initial (`esi_start=None`) or "More" (`esi_start=Some,
/// pct`). Both paths share `run_playback` for the cpal stream.
/// Continuation burst (RaptorQ "More"): encodes `count` packets starting
/// at `esi_start`, reusing the same envelope/session/mime as the initial
/// burst. The RX recognises the session by `session_id` (deterministic
/// from envelope + profile), so as long as the source file + profile +
/// callsign + filename match the initial TX, the new ESIs land in the
/// same disk session.
pub fn spawn_more(
    avif_path: PathBuf,
    mode: String,
    callsign: String,
    filename: String,
    device_name: String,
    _save_dir: PathBuf,
    esi_start: u32,
    count: u32,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    tx_sink: Arc<dyn SampleSink>,
    ptt: SharedPtt,
    sink: Arc<dyn EventSink>,
    // Where to dump the synthesised WAV (mono, 48 kHz, int16) before
    // playback. `None` = legacy behaviour (no on-disk copy). The path
    // is built as `<save_wav_dir>/tx_history/tx-{ts}-{filename}.wav`
    // so it sits next to the source archived by `archive_payload`.
    save_wav_dir: Option<PathBuf>,
) -> TxHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        let samples = match encode_in_process(
            &avif_path,
            &mode,
            &callsign,
            &filename,
            esi_start,
            count,
        ) {
            Ok(s) => s,
            Err(e) => {
                sink.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
        let total_blocks = count;
        sink.emit(
            "tx_plan",
            TxPlanEvent {
                duration_s,
                total_blocks,
                wire_bytes: 0,
                wav_path: String::new(),
                mode: mode.clone(),
                callsign: callsign.clone(),
                filename: filename.clone(),
            },
        );
        let wav_path = save_wav_dir
            .as_deref()
            .map(|d| build_tx_wav_path(d, &filename, esi_start))
            .unwrap_or_default();
        run_playback(
            &device_name,
            samples,
            total_blocks,
            duration_s,
            wav_path,
            attenuation_db,
            preemphasis_enabled,
            tx_sink,
            stop_thread,
            ptt,
            sink,
        );
    });
    TxHandle {
        stop,
        thread: Some(thread),
    }
}

/// Initial TX burst: encodes K source + repair packets so the RX has
/// enough redundancy to fountain-decode without retransmission, on a
/// loss-free channel.
pub fn spawn(
    avif_path: PathBuf,
    mode: String,
    callsign: String,
    filename: String,
    device_name: String,
    _save_dir: PathBuf,
    repair_pct: u32,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    tx_sink: Arc<dyn SampleSink>,
    ptt: SharedPtt,
    sink: Arc<dyn EventSink>,
    // See `spawn_more::save_wav_dir`. Same semantics for the initial
    // burst.
    save_wav_dir: Option<PathBuf>,
) -> TxHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        let payload_bytes = match std::fs::metadata(&avif_path).map(|m| m.len() as usize) {
            Ok(n) => n,
            Err(e) => {
                sink.emit(
                    "tx_error",
                    TxErrorEvent {
                        message: format!("source absent ou inaccessible: {e}"),
                    },
                );
                return;
            }
        };
        let plan = match tx_plan(
            payload_bytes,
            &mode,
            callsign.len(),
            filename.len(),
            repair_pct,
        ) {
            Ok(p) => p,
            Err(e) => {
                sink.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let total_blocks = plan.n_initial;

        let samples = match encode_in_process(
            &avif_path,
            &mode,
            &callsign,
            &filename,
            0,
            total_blocks,
        ) {
            Ok(s) => s,
            Err(e) => {
                sink.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
        let envelope_overhead = 2 + filename.len() + 1 + callsign.len() + 1;
        let wire_bytes = (payload_bytes + envelope_overhead) as u32;

        let wav_path = save_wav_dir
            .as_deref()
            .map(|d| build_tx_wav_path(d, &filename, 0))
            .unwrap_or_default();
        sink.emit(
            "tx_plan",
            TxPlanEvent {
                duration_s,
                total_blocks,
                wire_bytes,
                wav_path: wav_path.clone(),
                mode: mode.clone(),
                callsign: callsign.clone(),
                filename: filename.clone(),
            },
        );

        run_playback(
            &device_name,
            samples,
            total_blocks,
            duration_s,
            wav_path,
            attenuation_db,
            preemphasis_enabled,
            tx_sink,
            stop_thread,
            ptt,
            sink,
        );
    });
    TxHandle {
        stop,
        thread: Some(thread),
    }
}

/// Read the source file, build the wire payload (envelope-wrapped),
/// derive session_id + hash + mime, and call `V3Modem.encode_to_samples`
/// to produce the audio. Used for both the initial burst (esi_start=0)
/// and the "More" continuation burst (esi_start>0).
fn encode_in_process(
    avif_path: &Path,
    mode: &str,
    callsign: &str,
    filename: &str,
    esi_start: u32,
    n_packets: u32,
) -> Result<Vec<f32>, String> {
    let payload = std::fs::read(avif_path)
        .map_err(|e| format!("read {}: {e}", avif_path.display()))?;
    let mime_type = infer_mime(avif_path);
    let envelope = PayloadEnvelope::new(filename, callsign, payload).ok_or_else(|| {
        format!(
            "envelope too large (filename={}, callsign={})",
            filename.len(),
            callsign.len()
        )
    })?;
    let wire_payload = envelope.encode();

    let cfg = parse_profile(mode)?;
    let profile_index = profile_index_for(mode)
        .ok_or_else(|| format!("unknown profile '{mode}'"))?;
    let session_id =
        app_header::compute_session_id(&wire_payload, cfg.mode_code(), profile_index);
    let hash_short = fnv_short(&wire_payload);

    let req = EncodeRequest {
        profile: mode,
        wire_payload: &wire_payload,
        session_id,
        mime_type,
        hash_short,
        esi_start,
        n_packets,
        vox_seconds: TX_VOX_SECONDS,
    };
    V3Modem
        .encode_to_samples(&req)
        .map_err(|e| format!("encode: {e}"))
}

// run_playback / ptt_engage / ptt_release / archive_payload /
// purge_history / build_tx_wav_path / write_tx_wav / sanitize_filename
// all moved verbatim to modem-worker-base::tx_runtime. They reach this
// module's call sites through the `pub use modem_worker_base::tx_runtime::…`
// re-export at the top.
