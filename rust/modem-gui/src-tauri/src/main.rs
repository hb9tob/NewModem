#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod audio_capture;
mod collector_client;
mod ptt;
mod rx_worker;
mod session_store;
mod settings;
mod tx_encode;
mod tx_worker;

use audio::{list_input_devices, list_output_devices, DeviceInfo};
use ptt::SharedPtt;
use settings::Settings;
use tx_worker::TxHandle;
use audio_capture::CaptureHandle;
use rx_worker::{SharedWavSink, WavSink, WorkerHandle};
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
    /// Chemin de la payload prête à émettre (`tx_preview.avif` ou
    /// `tx_preview.zst`). Renseigné par compress_image / compress_file_zstd.
    /// `tx_start` lit ce chemin pour piloter le CLI.
    tx_payload_path: Arc<Mutex<Option<PathBuf>>>,
    tx_handle: Mutex<Option<TxHandle>>,
    ptt: SharedPtt,
}

#[derive(serde::Serialize, Clone)]
struct PttStatusEvent {
    /// "ok" : port ouvert, lignes en RX. "off" : désactivée par config.
    /// "error" : ouverture échouée — `message` détaille.
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
fn get_settings() -> Settings {
    settings::load()
}

#[tauri::command]
fn save_settings(
    settings: Settings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    settings::save(&settings)?;
    let status = compute_ptt_status(&state.ptt, &settings);
    let _ = app.emit("ptt_status", status);
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
        other => return Err(format!("unknown profile '{other}'")),
    };
    let (capture, samples) = audio_capture::start(&device_name)?;
    let worker = rx_worker::spawn(
        samples,
        app.clone(),
        state.save_dir.clone(),
        state.wav_sink.clone(),
        profile_idx,
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

/// Submit a finished raw capture to the Phase D collector. URL et HMAC
/// sont gérés dans `collector_client`. Async parce que reqwest l'est ;
/// Tauri 2 sait gérer async commands.
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
    /// Durée audio pour émettre n_initial (K + repair). Valeur de référence
    /// pour la barre de progression et le garde-fou "TX > 5 min".
    duration_s: f64,
    /// Nombre total de blocs émis par le burst initial (= K + repair).
    total_blocks: u32,
    /// Blocs RaptorQ nécessaires au décodage (K source-symbols).
    k_source: u32,
    /// Blocs réellement émis par le TX initial (= K + repair, redondant avec
    /// total_blocks mais explicite pour l'UI).
    n_initial: u32,
    /// Durée minimale théorique si zéro paquet n'était perdu (K seul).
    duration_s_k: f64,
    /// Durée d'un codeword, utilisé côté UI pour dériver la durée "+N%" du
    /// bouton More.
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
    let history_max = cfg.tx_history_max;
    let repair_pct = args.repair_pct.unwrap_or(30);
    tx_worker::archive_payload(
        &save_dir,
        &payload_path,
        &args.mode,
        &args.filename,
        repair_pct,
        history_max,
        &app,
    );
    let handle = tx_worker::spawn(
        payload_path,
        args.mode,
        args.callsign.trim().to_uppercase(),
        args.filename,
        args.tx_device,
        save_dir,
        repair_pct,
        attenuation_db,
        state.ptt.clone(),
        app,
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
    let attenuation_db = settings::load().tx_attenuation_db;
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
        state.ptt.clone(),
        app,
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
// Vue unifiée des fichiers TX (archivés au lancement de chaque émission par
// `tx_worker::archive_payload`) et RX (sessions décodées par session_store).
// Sert le mode radio-secours : un opérateur reçoit un fichier et peut le
// re-émettre d'un clic pour le propager plus loin sur le réseau.

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
    /// Chemin pour la vignette (asset:// affichable). Toujours pointé sur le
    /// fichier décompressé/affichable s'il existe (sinon le decoded.<ext>
    /// brut du session_dir).
    preview_path: String,
    /// Chemin à passer à `set_tx_source_from_path` pour relayer (radio-secours).
    /// AVIF → `decoded.avif` (passthrough bit-à-bit) ; sinon copie root.
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
        // Trouve le fichier source jumeau (avif/zst/bin) — pas forcément même
        // extension que celle déduite du mime_type, on parcourt le dossier.
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
            meta.mime_type == modem_core::app_header::mime::IMAGE_AVIF
                || meta.mime_type == modem_core::app_header::mime::IMAGE_JPEG
                || meta.mime_type == modem_core::app_header::mime::IMAGE_PNG;
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
        let is_image = mime == modem_core::app_header::mime::IMAGE_AVIF
            || mime == modem_core::app_header::mime::IMAGE_JPEG
            || mime == modem_core::app_header::mime::IMAGE_PNG;
        let display_filename = meta.filename.clone().unwrap_or_else(|| {
            format!("session-{:08x}.bin", meta.session_id)
        });
        let root_copy = save_dir.join(&display_filename);
        // Preview ET relay : on utilise toujours la copie root écrite par
        // rx_worker::emit_decoded_file. C'est la SEULE version contenant le
        // fichier extrait de la PayloadEnvelope (= AVIF/PNG/ZSTD pur). Le
        // fichier `decoded.<ext>` du session_dir contient l'envelope brute
        // (header + content) et n'est pas exploitable directement comme
        // image ou comme payload TX.
        if !root_copy.exists() {
            // Session décodée mais copie root absente (cleanup manuel ou
            // sessions héritées d'une version antérieure) — on l'ignore.
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
            // Garde-fou : doit être dans <save_dir>/tx_history/.
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
            // Supprime aussi la copie root si on retrouve le filename via meta.
            // Best-effort : on ne lit plus le meta.json (déjà rm'd) — on laisse
            // la copie root en place si l'utilisateur l'a déjà déplacée ou
            // copiée ailleurs, c'est plus prudent que d'effacer à l'aveugle.
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

/// Compresse la source (chargée via set_tx_source_from_path) en zstd niveau
/// max et écrit le résultat dans `tx_preview.zst`. Pour les fichiers non-image
/// (texte, archives, etc.) où on veut une transmission sans perte.
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

fn main() {
    let save_dir = default_save_dir();
    let _ = std::fs::create_dir_all(&save_dir);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let ptt: SharedPtt = Arc::new(Mutex::new(None));
            // Tentative d'ouverture du port PTT au démarrage. Si KO, on émet
            // un événement et on laisse `ptt` à None pour la session — l'UI
            // peut toujours rouvrir via Paramètres → save_settings.
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
