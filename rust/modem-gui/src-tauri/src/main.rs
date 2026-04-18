#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod audio_capture;
mod rx_worker;

use audio::{list_input_devices, DeviceInfo};
use audio_capture::CaptureHandle;
use rx_worker::WorkerHandle;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};

struct CaptureSession {
    capture: CaptureHandle,
    worker: WorkerHandle,
    device_name: String,
}

struct AppState {
    session: Mutex<Option<CaptureSession>>,
    save_dir: Arc<Mutex<PathBuf>>,
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
    let worker = rx_worker::spawn(samples, app.clone(), state.save_dir.clone());
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
    Ok(())
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
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_audio_devices,
            start_capture,
            stop_capture,
            get_save_dir,
            set_save_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
