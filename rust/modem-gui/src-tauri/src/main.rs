#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;

use audio::{list_input_devices, DeviceInfo};

#[tauri::command]
fn list_audio_devices() -> Result<Vec<DeviceInfo>, String> {
    list_input_devices().map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![list_audio_devices])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
