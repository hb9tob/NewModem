#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod audio_capture;
mod rx_worker;
mod settings;
mod tx_encode;

use audio::{list_input_devices, list_output_devices, DeviceInfo};
use settings::Settings;
use audio_capture::CaptureHandle;
use rx_worker::{SharedWavSink, WavSink, WorkerHandle};
use tx_encode::{compress_avif, CompressOpts};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};

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
}

fn default_save_dir() -> PathBuf {
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
fn save_settings(settings: Settings) -> Result<(), String> {
    settings::save(&settings)
}

#[tauri::command]
fn start_capture(
    device_name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("capture already running".into());
    }
    let (capture, samples) = audio_capture::start(&device_name)?;
    let worker = rx_worker::spawn(
        samples,
        app.clone(),
        state.save_dir.clone(),
        state.wav_sink.clone(),
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

#[derive(serde::Serialize)]
struct CompressResult {
    preview_path: String,
    source_w: u32,
    source_h: u32,
    actual_w: u32,
    actual_h: u32,
    byte_len: usize,
}

#[tauri::command]
fn set_tx_source(bytes: Vec<u8>, state: State<'_, AppState>) -> Result<usize, String> {
    let len = bytes.len();
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = Some(bytes);
    Ok(len)
}

#[tauri::command]
fn clear_tx_source(state: State<'_, AppState>) -> Result<(), String> {
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = None;
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
    Ok(CompressResult {
        preview_path: path.to_string_lossy().into_owned(),
        source_w: result.source_w,
        source_h: result.source_h,
        actual_w: result.actual_w,
        actual_h: result.actual_h,
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
        .setup(move |app| {
            app.manage(AppState {
                session: Mutex::new(None),
                save_dir: Arc::new(Mutex::new(save_dir.clone())),
                wav_sink: Arc::new(Mutex::new(None)),
                tx_source: Arc::new(Mutex::new(None)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_audio_devices,
            list_output_audio_devices,
            get_settings,
            save_settings,
            start_capture,
            stop_capture,
            get_save_dir,
            set_save_dir,
            start_raw_recording,
            stop_raw_recording,
            is_raw_recording,
            set_tx_source,
            clear_tx_source,
            compress_image,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
