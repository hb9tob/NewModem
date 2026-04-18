//! cpal helpers — list input devices and expose them to the Tauri frontend.

use cpal::traits::{DeviceTrait, HostTrait};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub default_sample_rate: u32,
    pub is_default: bool,
}

pub fn list_input_devices() -> Result<Vec<DeviceInfo>, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok());

    let mut out = Vec::new();
    for device in host.input_devices()? {
        let name = match device.name() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let default_sample_rate = device
            .default_input_config()
            .map(|c| c.sample_rate().0)
            .unwrap_or(0);
        let is_default = default_name.as_deref() == Some(name.as_str());
        out.push(DeviceInfo {
            name,
            default_sample_rate,
            is_default,
        });
    }
    Ok(out)
}
