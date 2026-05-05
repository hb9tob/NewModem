#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod collector_client;
mod ptt;
mod settings;
mod tx_encode;

use modem_worker::session_store;
use modem_worker::{rx_worker, tx_worker, EventSink};

use modem_io::cpal_capture::{self, CaptureHandle};
use modem_io::devices::{list_input_devices, list_output_devices, DeviceInfo};
use ptt::SharedPtt;
use settings::Settings;
use modem_worker::tx_worker::TxHandle;
use modem_worker::rx_worker::{SharedWavSink, WavSink, WorkerHandle};
use tx_encode::{compress_avif, compress_zstd, CompressOpts};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

struct CaptureSession {
    capture: CaptureHandle,
    worker: WorkerHandle,
    device_name: String,
}

struct AppState {
    session: Mutex<Option<CaptureSession>>,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    tx_source: Arc<Mutex<Option<Vec<u8>>>>,
    /// Path of the payload ready to transmit (`tx_preview.avif` or
    /// `tx_preview.zst`). Filled in by compress_image / compress_file_zstd.
    /// `tx_start` reads this path to drive the CLI.
    tx_payload_path: Arc<Mutex<Option<PathBuf>>>,
    tx_handle: Mutex<Option<TxHandle>>,
    ptt: SharedPtt,
}

#[derive(serde::Serialize, Clone)]
struct PttStatusEvent {
    /// "ok": port open, lines in RX. "off": disabled by config.
    /// "error": open failed - `message` has the details.
    state: &'static str,
    message: String,
}

fn default_save_dir() -> PathBuf {
    if let Some(root) = settings::portable_root() {
        return root.join("nbfm-rx");
    }
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nbfm-rx")
}

#[tauri::command]
fn list_audio_devices() -> Result<Vec<DeviceInfo>, String> {
    list_input_devices().map_err(|e| e.to_string())
}

#[tauri::command]
fn list_output_audio_devices() -> Result<Vec<DeviceInfo>, String> {
    list_output_devices().map_err(|e| e.to_string())
}

#[tauri::command]
fn list_modem_profiles() -> Vec<modem_core::traits::ProfileDescriptor> {
    use modem_core::traits::Modem;
    modem_core::v3_modem::V3Modem.list_profiles()
}

#[tauri::command]
fn get_settings() -> Settings {
    settings::load()
}

#[tauri::command]
fn save_settings(
    settings: Settings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // Persist before deciding whether to touch the PTT — even if we
    // skip the refresh, the disk file must be up to date.
    let previous = settings::load();
    settings::save(&settings)?;
    // Re-opening the serial port toggles DTR/RTS for a few ms (OS-level
    // behavior of `serialport-rs`), which keys the radio for a short
    // burst. The frontend calls `save_settings` on every settings change
    // (including AVIF quality / speed sliders), so we only refresh PTT
    // when a PTT-relevant field actually changed. Polarity changes are
    // included because PttController applies them at open time.
    let ptt_changed = previous.ptt_enabled != settings.ptt_enabled
        || previous.ptt_port != settings.ptt_port
        || previous.ptt_use_rts != settings.ptt_use_rts
        || previous.ptt_use_dtr != settings.ptt_use_dtr
        || previous.ptt_rts_tx_high != settings.ptt_rts_tx_high
        || previous.ptt_dtr_tx_high != settings.ptt_dtr_tx_high;
    // Recovery path: if PTT is enabled but no controller is currently
    // open (e.g. startup failed because the port was busy), allow any
    // settings save to retry the open. A failed open doesn't toggle the
    // serial lines, so this stays silent on the radio.
    let needs_recovery = settings.ptt_enabled
        && state
            .ptt
            .lock()
            .ok()
            .map(|g| g.is_none())
            .unwrap_or(false);
    if ptt_changed || needs_recovery {
        let status = compute_ptt_status(&state.ptt, &settings);
        let _ = app.emit("ptt_status", status);
    }
    Ok(())
}

fn compute_ptt_status(slot: &SharedPtt, settings: &Settings) -> PttStatusEvent {
    match ptt::refresh(slot, settings) {
        Ok(Some(msg)) => PttStatusEvent {
            state: "ok",
            message: msg,
        },
        Ok(None) => PttStatusEvent {
            state: "off",
            message: "PTT désactivée".to_string(),
        },
        Err(e) => PttStatusEvent {
            state: "error",
            message: format!("PTT indisponible : {e}"),
        },
    }
}

#[tauri::command]
fn list_serial_ports() -> Vec<String> {
    ptt::list_ports()
}

#[tauri::command]
fn ptt_status(state: State<'_, AppState>) -> PttStatusEvent {
    let settings = settings::load();
    if !settings.ptt_enabled {
        return PttStatusEvent {
            state: "off",
            message: "PTT désactivée".to_string(),
        };
    }
    let active = state
        .ptt
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|c| c.port_name().to_string()));
    match active {
        Some(name) => PttStatusEvent {
            state: "ok",
            message: format!("PTT prête sur {name}"),
        },
        None => PttStatusEvent {
            state: "error",
            message: "PTT configurée mais port indisponible".to_string(),
        },
    }
}

#[tauri::command]
fn start_capture(
    device_name: String,
    profile: Option<String>,
    forced: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("capture already running".into());
    }
    let profile_idx = match profile.as_deref().unwrap_or("HIGH").to_uppercase().as_str() {
        "MEGA" => modem_core::profile::ProfileIndex::Mega,
        "HIGH" => modem_core::profile::ProfileIndex::High,
        "NORMAL" => modem_core::profile::ProfileIndex::Normal,
        "ROBUST" => modem_core::profile::ProfileIndex::Robust,
        "ULTRA" => modem_core::profile::ProfileIndex::Ultra,
        // EXPERIMENTAL : seulement utilisable si forced=true.
        "HIGH+" | "HIGHPLUS" => modem_core::profile::ProfileIndex::HighPlus,
        "FAST" => modem_core::profile::ProfileIndex::Fast,
        "HIGH++" | "HIGHPLUSPLUS" => modem_core::profile::ProfileIndex::HighPlusPlus,
        "HIGH56" | "HIGH-56" => modem_core::profile::ProfileIndex::HighFiveSix,
        "HIGH+56" | "HIGHPLUS56" => modem_core::profile::ProfileIndex::HighPlusFiveSix,
        other => return Err(format!("unknown profile '{other}'")),
    };
    let forced = forced.unwrap_or(false);
    if profile_idx.is_experimental() && !forced {
        return Err(format!(
            "profil '{}' est expérimental, requiert forced=true",
            profile_idx.name()
        ));
    }
    let (capture, samples) = cpal_capture::start(&device_name)?;
    let sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app.clone()));
    let cfg = settings::load();
    let worker = rx_worker::spawn(
        samples,
        sink,
        state.save_dir.clone(),
        state.wav_sink.clone(),
        profile_idx,
        forced,
        cfg.rx_deemphasis_enabled,
    );
    *guard = Some(CaptureSession {
        capture,
        worker,
        device_name,
    });
    Ok(())
}

#[tauri::command]
fn stop_capture(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|e| e.to_string())?;
    if let Some(session) = guard.take() {
        // Drop the capture first : that closes the stream and disconnects
        // the mpsc channel, so the worker's recv() returns and the thread
        // exits naturally.
        session.capture.stop();
        session.worker.stop();
    }
    // If a raw recording was still armed, finalize it now so the WAV is
    // closed properly. We don't error out if it fails — audio capture is
    // already stopped.
    if let Ok(mut sink_guard) = state.wav_sink.lock() {
        if let Some(sink) = sink_guard.take() {
            let _ = sink.finalize();
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct RawRecordingStatus {
    path: String,
    samples: u64,
    duration_sec: f64,
}

/// Arm raw-audio capture. Returns the absolute path of the new WAV. Fails if
/// a recording is already in progress.
#[tauri::command]
fn start_raw_recording(state: State<'_, AppState>) -> Result<String, String> {
    let mut sink_guard = state.wav_sink.lock().map_err(|e| e.to_string())?;
    if sink_guard.is_some() {
        return Err("raw recording already in progress".into());
    }
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("capture-{ts}.wav"));
    let sink = WavSink::create(&path).map_err(|e| format!("wav create: {e}"))?;
    let path_str = sink.path.to_string_lossy().into_owned();
    *sink_guard = Some(sink);
    Ok(path_str)
}

/// Finalise raw-audio capture. Returns the WAV path and number of samples
/// written. Fails if no recording was active.
#[tauri::command]
fn stop_raw_recording(state: State<'_, AppState>) -> Result<RawRecordingStatus, String> {
    let mut sink_guard = state.wav_sink.lock().map_err(|e| e.to_string())?;
    let sink = sink_guard
        .take()
        .ok_or_else(|| "no raw recording in progress".to_string())?;
    let (path, samples) = sink.finalize().map_err(|e| format!("wav finalize: {e}"))?;
    Ok(RawRecordingStatus {
        path: path.to_string_lossy().into_owned(),
        samples,
        duration_sec: samples as f64 / 48_000.0,
    })
}

#[tauri::command]
fn is_raw_recording(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state
        .wav_sink
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false))
}

/// Submit a finished raw capture to the Phase D collector. URL and HMAC
/// are handled inside `collector_client`. Async because reqwest is;
/// Tauri 2 handles async commands.
#[tauri::command]
async fn submit_capture(
    args: collector_client::SubmitCaptureArgs,
) -> Result<collector_client::SubmitResult, String> {
    collector_client::submit(args).await
}

#[derive(serde::Serialize)]
struct CompressResult {
    preview_path: String,
    source_w: u32,
    source_h: u32,
    actual_w: u32,
    actual_h: u32,
    byte_len: usize,
}

#[derive(serde::Serialize)]
struct TxEstimate {
    /// Audio duration to transmit n_initial (K + repair). Reference
    /// value for the progress bar and the "TX > 5 min" guard.
    duration_s: f64,
    /// Total number of blocks emitted by the initial burst (= K + repair).
    total_blocks: u32,
    /// RaptorQ blocks required to decode (K source symbols).
    k_source: u32,
    /// Blocks actually emitted by the initial TX (= K + repair, redundant
    /// with total_blocks but explicit for the UI).
    n_initial: u32,
    /// Minimum theoretical duration if zero packets were lost (K only).
    duration_s_k: f64,
    /// Duration of a single codeword, used UI-side to derive the "+N%"
    /// duration of the More button.
    seconds_per_cw: f64,
}

#[tauri::command]
fn tx_estimate(
    payload_bytes: usize,
    mode: String,
    callsign: String,
    filename: String,
    repair_pct: Option<u32>,
) -> Result<TxEstimate, String> {
    let plan = tx_worker::tx_plan(
        payload_bytes,
        &mode,
        callsign.len(),
        filename.len(),
        repair_pct.unwrap_or(30),
    )?;
    Ok(TxEstimate {
        duration_s: plan.duration_s_initial,
        total_blocks: plan.n_initial,
        k_source: plan.k_source,
        n_initial: plan.n_initial,
        duration_s_k: plan.duration_s_k,
        seconds_per_cw: plan.seconds_per_cw,
    })
}

#[derive(serde::Deserialize)]
struct TxStartArgs {
    mode: String,
    callsign: String,
    filename: String,
    tx_device: String,
    /// RaptorQ repair-pct chosen in the GUI (0, 5, 10, 20, 30, 50, 100...).
    /// Defaults to 30 when the caller omits it.
    #[serde(default)]
    repair_pct: Option<u32>,
}

#[tauri::command]
fn tx_start(
    args: TxStartArgs,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if tx_guard.is_some() {
        return Err("TX déjà en cours".into());
    }
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let payload_path = state
        .tx_payload_path
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "aucune payload prête (compresse d'abord)".to_string())?;
    if !payload_path.exists() {
        return Err(format!(
            "payload absent ({}), recompresse avant TX",
            payload_path.display()
        ));
    }
    if args.callsign.trim().is_empty() {
        return Err("indicatif vide (Paramètres → Indicatif)".into());
    }
    if args.tx_device.trim().is_empty() {
        return Err("carte son TX non sélectionnée (Paramètres)".into());
    }
    let cfg = settings::load();
    let attenuation_db = cfg.tx_attenuation_db;
    let preemphasis_enabled = cfg.tx_preemphasis_enabled;
    let history_max = cfg.tx_history_max;
    let repair_pct = args.repair_pct.unwrap_or(30);
    let archive_sink = TauriEventSink(app.clone());
    tx_worker::archive_payload(
        &save_dir,
        &payload_path,
        &args.mode,
        &args.filename,
        repair_pct,
        history_max,
        &archive_sink,
    );
    let tx_sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app));
    let handle = tx_worker::spawn(
        payload_path,
        args.mode,
        args.callsign.trim().to_uppercase(),
        args.filename,
        args.tx_device,
        save_dir,
        repair_pct,
        attenuation_db,
        preemphasis_enabled,
        state.ptt.clone(),
        tx_sink,
    );
    *tx_guard = Some(handle);
    Ok(())
}

#[derive(serde::Deserialize)]
struct TxMoreArgs {
    mode: String,
    callsign: String,
    filename: String,
    tx_device: String,
    esi_start: u32,
    /// Exact number of additional blocks to emit. The UI picks it directly
    /// (dropdown / free input) — no more percentage conversion, so "I'm
    /// missing 5 blocks" translates 1:1 to `count = 5`.
    count: u32,
}

#[tauri::command]
fn tx_more(
    args: TxMoreArgs,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if tx_guard.is_some() {
        return Err("TX déjà en cours".into());
    }
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let payload_path = state
        .tx_payload_path
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "aucune payload prête (compresse d'abord)".to_string())?;
    if !payload_path.exists() {
        return Err(format!(
            "payload absent ({}), recompresse avant TX",
            payload_path.display()
        ));
    }
    if args.callsign.trim().is_empty() {
        return Err("indicatif vide (Paramètres → Indicatif)".into());
    }
    if args.tx_device.trim().is_empty() {
        return Err("carte son TX non sélectionnée (Paramètres)".into());
    }
    if args.count == 0 {
        return Err("choisir un nombre de blocs > 0".into());
    }
    let cfg = settings::load();
    let attenuation_db = cfg.tx_attenuation_db;
    let preemphasis_enabled = cfg.tx_preemphasis_enabled;
    let tx_sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app));
    let handle = tx_worker::spawn_more(
        payload_path,
        args.mode,
        args.callsign.trim().to_uppercase(),
        args.filename,
        args.tx_device,
        save_dir,
        args.esi_start,
        args.count,
        attenuation_db,
        preemphasis_enabled,
        state.ptt.clone(),
        tx_sink,
    );
    *tx_guard = Some(handle);
    Ok(())
}

#[tauri::command]
fn list_sessions(state: State<'_, AppState>) -> Result<Vec<session_store::SessionMeta>, String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let store = session_store::SessionStore::new(&save_dir).map_err(|e| e.to_string())?;
    Ok(store.list_all())
}

#[tauri::command]
fn delete_session(session_id: u32, state: State<'_, AppState>) -> Result<(), String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let dir = save_dir
        .join("sessions")
        .join(format!("{session_id:08x}.session"));
    if !dir.exists() {
        return Err(format!("session {session_id:08x} absente"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("rm {}: {e}", dir.display()))
}

// ─────────────────────────────────────────── Onglet Historique
//
// Unified view of TX files (archived at TX launch by
// `tx_worker::archive_payload`) and RX files (sessions decoded by
// `session_store`). Powers the radio-rescue mode: an operator receives
// a file and can re-emit it with one click to forward it further on the
// network.

#[derive(serde::Serialize)]
struct TxHistoryItem {
    timestamp: i64,
    mode: String,
    mime_type: u8,
    filename: String,
    file_path: String,
    is_image: bool,
    size_bytes: u64,
}

#[derive(serde::Serialize)]
struct RxHistoryItem {
    session_id: String,
    timestamp: i64,
    callsign: Option<String>,
    filename: String,
    /// Path for the thumbnail (`asset://`-displayable). Always points to
    /// the decompressed/displayable file when it exists (otherwise the
    /// raw `decoded.<ext>` from the session_dir).
    preview_path: String,
    /// Path to pass to `set_tx_source_from_path` for relaying
    /// (radio rescue). AVIF -> `decoded.avif` (bit-for-bit passthrough);
    /// otherwise the root copy.
    relay_path: String,
    is_image: bool,
    size_bytes: u64,
    mode: String,
    mime_type: u8,
}

#[derive(serde::Deserialize)]
struct TxHistoryMetaRead {
    timestamp: i64,
    mode: String,
    mime_type: u8,
    filename: String,
}

#[tauri::command]
fn list_tx_history(state: State<'_, AppState>) -> Result<Vec<TxHistoryItem>, String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let dir = save_dir.join("tx_history");
    let mut items: Vec<TxHistoryItem> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(items), // dossier absent = historique vide, pas une erreur
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else { continue };
        if !ext.eq_ignore_ascii_case("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(meta) = serde_json::from_str::<TxHistoryMetaRead>(&raw) else { continue };
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Find the twin source file (avif/zst/bin) - not necessarily the
        // same extension as the one derived from mime_type, so we walk
        // the directory.
        let mut payload_path: Option<PathBuf> = None;
        for sib in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = sib.path();
            if p.file_stem().and_then(|s| s.to_str()) == Some(&stem)
                && p.extension().and_then(|s| s.to_str())
                    .map(|e| !e.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
            {
                payload_path = Some(p);
                break;
            }
        }
        let Some(payload) = payload_path else { continue };
        let size_bytes = payload.metadata().map(|m| m.len()).unwrap_or(0);
        let is_image =
            meta.mime_type == modem_framing::app_header::mime::IMAGE_AVIF
                || meta.mime_type == modem_framing::app_header::mime::IMAGE_JPEG
                || meta.mime_type == modem_framing::app_header::mime::IMAGE_PNG;
        items.push(TxHistoryItem {
            timestamp: meta.timestamp,
            mode: meta.mode,
            mime_type: meta.mime_type,
            filename: meta.filename,
            file_path: payload.to_string_lossy().into_owned(),
            is_image,
            size_bytes,
        });
    }
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(items)
}

#[tauri::command]
fn list_rx_history(state: State<'_, AppState>) -> Result<Vec<RxHistoryItem>, String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let store = session_store::SessionStore::new(&save_dir).map_err(|e| e.to_string())?;
    let mut items: Vec<RxHistoryItem> = Vec::new();
    for meta in store.list_all().into_iter().filter(|m| m.decoded) {
        let mime = meta.mime_type;
        let is_image = mime == modem_framing::app_header::mime::IMAGE_AVIF
            || mime == modem_framing::app_header::mime::IMAGE_JPEG
            || mime == modem_framing::app_header::mime::IMAGE_PNG;
        let display_filename = meta.filename.clone().unwrap_or_else(|| {
            format!("session-{:08x}.bin", meta.session_id)
        });
        let root_copy = save_dir.join(&display_filename);
        // Preview AND relay: we always use the root copy written by
        // `rx_worker::emit_decoded_file`. That is the ONLY version that
        // contains the file extracted from the PayloadEnvelope (= pure
        // AVIF/PNG/ZSTD). The `decoded.<ext>` file in the session_dir
        // contains the raw envelope (header + content) and cannot be
        // used directly as an image or as a TX payload.
        if !root_copy.exists() {
            // Session decoded but root copy missing (manual cleanup or
            // sessions inherited from an older version) - skip it.
            continue;
        }
        let preview = root_copy.clone();
        let relay = root_copy.clone();
        let size_bytes = preview.metadata().map(|m| m.len()).unwrap_or(0);
        items.push(RxHistoryItem {
            session_id: format!("{:08x}", meta.session_id),
            timestamp: meta.created_at as i64,
            callsign: meta.callsign,
            filename: display_filename,
            preview_path: preview.to_string_lossy().into_owned(),
            relay_path: relay.to_string_lossy().into_owned(),
            is_image,
            size_bytes,
            mode: meta.profile,
            mime_type: mime,
        });
    }
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(items)
}

#[tauri::command]
fn delete_history_item(
    kind: String,
    key: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    match kind.as_str() {
        "tx" => {
            let path = PathBuf::from(&key);
            // Guard: must be inside <save_dir>/tx_history/.
            let history_dir = save_dir.join("tx_history");
            if !path.starts_with(&history_dir) {
                return Err("chemin hors tx_history/".into());
            }
            // Supprime le fichier + son metadata jumeau (.json).
            let _ = std::fs::remove_file(&path);
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let meta = history_dir.join(format!("{stem}.json"));
                let _ = std::fs::remove_file(&meta);
            }
            Ok(())
        }
        "rx" => {
            // key = session_id hex 8 chars.
            let session_id = u32::from_str_radix(&key, 16)
                .map_err(|e| format!("session_id invalide '{key}': {e}"))?;
            let dir = save_dir
                .join("sessions")
                .join(format!("{session_id:08x}.session"));
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .map_err(|e| format!("rm {}: {e}", dir.display()))?;
            }
            // We also remove the root copy if we can recover the filename
            // from the meta. Best-effort: we don't read meta.json any more
            // (already rm'd) - we leave the root copy in place if the user
            // has already moved or copied it elsewhere; safer than blind
            // deletion.
            Ok(())
        }
        other => Err(format!("kind inconnu '{other}' (tx|rx)")),
    }
}

#[tauri::command]
fn tx_stop(state: State<'_, AppState>) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if let Some(h) = tx_guard.take() {
        h.stop();
    }
    Ok(())
}

/// Called by JS once it has consumed tx_complete/tx_error. Cleans up the
/// handle slot so subsequent tx_start calls work.
#[tauri::command]
fn tx_reset(state: State<'_, AppState>) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if let Some(h) = tx_guard.take() {
        h.stop();
    }
    Ok(())
}

#[tauri::command]
fn set_tx_source(bytes: Vec<u8>, state: State<'_, AppState>) -> Result<usize, String> {
    let len = bytes.len();
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = Some(bytes);
    Ok(len)
}

// Path-based variant used by the drag-drop code path. Passing the raw bytes
// through IPC (set_tx_source) forces JSON-array serialization of the whole
// image, which allocates ~10× the file size between JS and Rust and was
// enough to OOM-freeze the desktop on large drops.
#[tauri::command]
fn set_tx_source_from_path(
    path: String,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
    let len = bytes.len();
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = Some(bytes);
    Ok(len)
}

#[tauri::command]
fn clear_tx_source(state: State<'_, AppState>) -> Result<(), String> {
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = None;
    if let Ok(mut p) = state.tx_payload_path.lock() {
        *p = None;
    }
    Ok(())
}

#[tauri::command]
fn compress_image(
    opts: CompressOpts,
    state: State<'_, AppState>,
) -> Result<CompressResult, String> {
    let source = {
        let slot = state.tx_source.lock().map_err(|e| e.to_string())?;
        slot.clone().ok_or_else(|| "no tx source loaded".to_string())?
    };
    let result = compress_avif(&source, &opts)?;
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("tx_preview.avif");
    std::fs::write(&path, &result.avif_bytes).map_err(|e| format!("write: {e}"))?;
    if let Ok(mut p) = state.tx_payload_path.lock() {
        *p = Some(path.clone());
    }
    Ok(CompressResult {
        preview_path: path.to_string_lossy().into_owned(),
        source_w: result.source_w,
        source_h: result.source_h,
        actual_w: result.actual_w,
        actual_h: result.actual_h,
        byte_len: result.byte_len,
    })
}

#[derive(serde::Serialize)]
struct CompressFileResult {
    preview_path: String,
    source_len: usize,
    byte_len: usize,
}

/// Compress the source (loaded via `set_tx_source_from_path`) with zstd
/// at max level and write the result to `tx_preview.zst`. For non-image
/// files (text, archives, etc.) that require lossless transmission.
#[tauri::command]
fn compress_file_zstd(state: State<'_, AppState>) -> Result<CompressFileResult, String> {
    let source = {
        let slot = state.tx_source.lock().map_err(|e| e.to_string())?;
        slot.clone().ok_or_else(|| "no tx source loaded".to_string())?
    };
    let result = compress_zstd(&source)?;
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("tx_preview.zst");
    std::fs::write(&path, &result.zst_bytes).map_err(|e| format!("write: {e}"))?;
    if let Ok(mut p) = state.tx_payload_path.lock() {
        *p = Some(path.clone());
    }
    Ok(CompressFileResult {
        preview_path: path.to_string_lossy().into_owned(),
        source_len: result.source_len,
        byte_len: result.byte_len,
    })
}

#[tauri::command]
fn get_save_dir(state: State<'_, AppState>) -> Result<String, String> {
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().into_owned())
}

#[tauri::command]
fn set_save_dir(path: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut dir = state.save_dir.lock().map_err(|e| e.to_string())?;
    let p = PathBuf::from(path);
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    *dir = p;
    Ok(())
}

/// Adapter that bridges `modem_worker::EventSink` onto a Tauri `AppHandle`,
/// so workers extracted into `modem-worker` can keep their existing event
/// names + payload shapes without depending on Tauri.
struct TauriEventSink(AppHandle);

impl EventSink for TauriEventSink {
    fn emit_json(&self, name: &str, payload: serde_json::Value) {
        // Same fire-and-forget semantics the workers used to have when
        // they called `app.emit(...)` directly.
        let _ = self.0.emit(name, payload);
    }
}

fn main() {
    let save_dir = default_save_dir();
    let _ = std::fs::create_dir_all(&save_dir);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let ptt: SharedPtt = Arc::new(Mutex::new(None));
            // Best-effort PTT port open at startup. On failure we emit an
            // event and leave `ptt` at None for the session - the UI can
            // still re-open it later via Settings -> save_settings.
            let startup_settings = settings::load();
            let status = compute_ptt_status(&ptt, &startup_settings);
            if status.state == "error" {
                eprintln!("[ptt] {}", status.message);
            }
            let _ = app.handle().emit("ptt_status", status);
            app.manage(AppState {
                session: Mutex::new(None),
                save_dir: Arc::new(Mutex::new(save_dir.clone())),
                wav_sink: Arc::new(Mutex::new(None)),
                tx_source: Arc::new(Mutex::new(None)),
                tx_payload_path: Arc::new(Mutex::new(None)),
                tx_handle: Mutex::new(None),
                ptt,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_audio_devices,
            list_output_audio_devices,
            list_modem_profiles,
            list_serial_ports,
            ptt_status,
            get_settings,
            save_settings,
            start_capture,
            stop_capture,
            get_save_dir,
            set_save_dir,
            start_raw_recording,
            stop_raw_recording,
            is_raw_recording,
            submit_capture,
            set_tx_source,
            set_tx_source_from_path,
            clear_tx_source,
            compress_image,
            compress_file_zstd,
            tx_estimate,
            tx_start,
            tx_more,
            tx_stop,
            tx_reset,
            list_sessions,
            delete_session,
            list_tx_history,
            list_rx_history,
            delete_history_item,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
