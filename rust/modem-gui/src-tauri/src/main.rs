#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod collector_client;
mod overlay;
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

/// Synthetic prefix the GUI uses to flag a Pluto entry in the
/// (otherwise cpal-only) device dropdown. The real libiio URI follows.
const PLUTO_DEVICE_PREFIX: &str = "pluto:";

/// Active RX backend. Dropping either variant releases the underlying
/// hardware; `stop_capture` matches on the variant to call the right
/// teardown.
enum CaptureKind {
    /// cpal soundcard input (the legacy default, used unless the
    /// device name carries the [`PLUTO_DEVICE_PREFIX`]).
    Cpal(CaptureHandle),
    /// PlutoSDR via libiio. The capture thread runs the radio-faithful
    /// demod chain from `modem-sdr-dsp`; the worker downstream sees
    /// the same 48 kHz mono `Vec<f32>` stream as the cpal path.
    Pluto(modem_pluto::rx::CaptureHandle),
}

impl CaptureKind {
    fn stop(self) {
        match self {
            CaptureKind::Cpal(h) => h.stop(),
            CaptureKind::Pluto(h) => h.stop(),
        }
    }
}

struct CaptureSession {
    capture: CaptureKind,
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

/// Pluto entry surfaced to the RX device dropdown. Names follow the
/// convention `pluto:<libiio-uri>` so the backend's `start_capture`
/// can route on the prefix without a separate "kind" parameter — that
/// keeps the existing `device_name: String` Tauri contract intact.
#[derive(Debug, Clone, serde::Serialize)]
struct PlutoDeviceInfo {
    /// Synthetic device name, e.g. `pluto:usb:1.6.5`. The frontend
    /// stores this in `currentSettings.rx_device` like any other
    /// device name; the backend recognises the `pluto:` prefix in
    /// `start_capture` and opens the SDR backend instead of cpal.
    name: String,
    /// Raw libiio URI (without the `pluto:` prefix). Useful when the
    /// frontend wants to display "USB 1.6.5" or "ip:pluto.local".
    uri: String,
    /// Vendor / model string libiio reports (typically
    /// `"PlutoSDR (ADALM-PLUTO)"` for an ADALM-PLUTO).
    description: String,
    /// Single line for the dropdown.
    friendly_name: String,
}

/// Scan USB libiio backends for connected Plutos. Network-mode Plutos
/// (`ip:pluto.local`) aren't listed here — those are entered manually
/// in Settings for now. Empty result + no error means "libiio works
/// but no Pluto plugged in".
#[tauri::command]
fn list_pluto_devices() -> Result<Vec<PlutoDeviceInfo>, String> {
    eprintln!("[pluto] list_pluto_devices invoked");
    let scan = match industrial_io::ScanContext::new_usb() {
        Ok(s) => s,
        Err(e) => {
            // libiio not loadable / USB backend disabled — surface as
            // an empty list rather than an error so the GUI just shows
            // "no Pluto found" instead of a red banner.
            eprintln!("[pluto] USB scan unavailable: {e}");
            return Ok(Vec::new());
        }
    };
    eprintln!("[pluto] scan ok, len = {}", scan.len());
    let mut out = Vec::new();
    for (uri, descr) in scan.iter() {
        eprintln!("[pluto] entry uri={uri:?} descr={descr:?}");
        // Only keep entries whose description matches a Pluto. libiio
        // can list other AD9361-class boards too — skip those.
        let is_pluto = descr.contains("PlutoSDR")
            || descr.contains("ADALM-PLUTO")
            || descr.contains("AD9363")
            || descr.contains("AD9361");
        if !is_pluto {
            eprintln!("[pluto]   -> skipped (not a Pluto)");
            continue;
        }
        let friendly_name = format!("Pluto SDR — {uri}");
        out.push(PlutoDeviceInfo {
            name: format!("pluto:{uri}"),
            uri,
            description: descr,
            friendly_name,
        });
    }
    eprintln!("[pluto] returning {} entries", out.len());
    Ok(out)
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
    let cfg = settings::load();
    // Route on the device-name prefix: Pluto entries arrive as
    // `pluto:<libiio-uri>` (built by `list_pluto_devices`). Anything
    // else is a cpal soundcard. Both branches emit the same shape:
    // mpsc::Receiver<Vec<f32>> at 48 kHz mono — the rx_worker doesn't
    // know which backend produced the samples.
    let (capture, samples) =
        if let Some(uri) = device_name.strip_prefix(PLUTO_DEVICE_PREFIX) {
            // Default to slow_attack if Settings ever carries an
            // unrecognised mode string — the chip would reject the
            // raw libiio write anyway, but defaulting saves the user
            // a trip to the Settings panel.
            let rx_mode = modem_pluto::device::RxGainMode::from_iio_str(
                &cfg.pluto_rx_gain_mode,
            )
            .unwrap_or(modem_pluto::device::RxGainMode::SlowAttack);
            let pcfg = modem_pluto::device::PlutoConfig {
                uri: uri.to_string(),
                rx_freq_hz: cfg.pluto_rx_freq_hz,
                tx_freq_hz: cfg.pluto_tx_freq_hz,
                rx_gain_mode: rx_mode,
                rx_gain_db: cfg.pluto_rx_gain_db,
                tx_attenuation_db: cfg.pluto_tx_attenuation_db,
                rf_bandwidth_hz: 200_000,
                prefer_low_rate: true,
            };
            let (h, rx) = modem_pluto::rx::start(&pcfg)
                .map_err(|e| format!("Pluto open ({uri}): {e}"))?;
            (CaptureKind::Pluto(h), rx)
        } else {
            let (h, rx) = cpal_capture::start(&device_name)?;
            (CaptureKind::Cpal(h), rx)
        };
    let sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app.clone()));
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
        // Drop the capture first : that closes the stream / cancels
        // the libiio buffer pump and disconnects the mpsc channel, so
        // the worker's recv() returns and the thread exits naturally.
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
fn overlays_import_logo(bytes: Vec<u8>, original_name: String) -> Result<String, String> {
    overlay::import_logo_bytes(&bytes, &original_name)
}

#[tauri::command]
fn overlays_logos_dir() -> Result<String, String> {
    let dir = overlay::logos_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().into_owned())
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

    // Install the default oscilloscope logo + pre-configure slot 1
    // (top-left corner, 0/0% margin, 10% height) the first time this
    // build runs against a given user profile. Triggers on both fresh
    // installs (no settings.json) and upgrades from a version that did
    // not have overlays. The `overlay_default_seeded` flag prevents
    // re-seeding on every launch and protects user customizations:
    // once set, we never touch slot 1 again. Slot 1 is only filled if
    // it is still empty, and `active_overlay` is only changed if the
    // user has not picked another slot.
    {
        let mut s = settings::load();
        if !s.overlay_default_seeded {
            match overlay::ensure_default_logo() {
                Ok(filename) => {
                    if let Some(slot) = s.overlays.get_mut(1) {
                        if slot.text.is_none() && slot.logo.is_none() {
                            slot.name = "NBFM Modem".to_string();
                            slot.logo = Some(overlay::LogoElement {
                                filename,
                                anchor: overlay::Anchor::TopLeft,
                                margin_x_pct: 0.0,
                                margin_y_pct: 0.0,
                                size_pct: 10.0,
                            });
                            if s.active_overlay == 0 {
                                s.active_overlay = 1;
                            }
                        }
                    }
                    s.overlay_default_seeded = true;
                    if let Err(e) = settings::save(&s) {
                        eprintln!("[overlay] could not save seeded settings: {e}");
                    }
                }
                Err(e) => eprintln!("[overlay] could not write default logo: {e}"),
            }
        }
    }

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
            // Auto-kiosk on tiny touchscreens (e.g. Pi 7" 800x480) or
            // when `NBFM_KIOSK=1` is set in the environment. Both paths
            // force a borderless fullscreen window and emit `kiosk_mode`
            // to the frontend so the CSS layout switches and the
            // on-screen exit button shows up. Diagnostic eprintlns are
            // intentional — running the binary from a terminal makes it
            // easy to see why the auto-detection did or didn't engage.
            let env_kiosk = std::env::var("NBFM_KIOSK")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false);
            if let Some(win) = app.get_webview_window("main") {
                let mon_kiosk = match win.primary_monitor() {
                    Ok(Some(monitor)) => {
                        let s = monitor.size();
                        let f = monitor.scale_factor().max(0.01);
                        let lw = (s.width as f64 / f) as u32;
                        let lh = (s.height as f64 / f) as u32;
                        eprintln!(
                            "[kiosk] monitor: phys {}x{} scale {:.2} -> logical {}x{}",
                            s.width, s.height, f, lw, lh
                        );
                        lw <= 900 || lh <= 600
                    }
                    Ok(None) => {
                        eprintln!("[kiosk] no primary monitor reported");
                        false
                    }
                    Err(e) => {
                        eprintln!("[kiosk] primary_monitor error: {e}");
                        false
                    }
                };
                if env_kiosk || mon_kiosk {
                    eprintln!(
                        "[kiosk] engaging (env={} mon={})",
                        env_kiosk, mon_kiosk
                    );
                    if let Err(e) = win.set_decorations(false) {
                        eprintln!("[kiosk] set_decorations err: {e}");
                    }
                    if let Err(e) = win.set_fullscreen(true) {
                        eprintln!("[kiosk] set_fullscreen err: {e}");
                    }
                    // Defer a second attempt: on Wayland (labwc), the
                    // toplevel surface may not be mapped yet when setup
                    // runs, so the first set_fullscreen can be a no-op.
                    let win2 = win.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        let _ = win2.set_decorations(false);
                        let _ = win2.set_fullscreen(true);
                    });
                    let _ = app.handle().emit("kiosk_mode", true);
                } else {
                    eprintln!("[kiosk] desktop mode (env={} mon={})", env_kiosk, mon_kiosk);
                }
            } else {
                eprintln!("[kiosk] no `main` window in setup hook");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_audio_devices,
            list_output_audio_devices,
            list_pluto_devices,
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
            overlays_import_logo,
            overlays_logos_dir,
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
