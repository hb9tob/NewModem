//! cpal helpers — list input devices and expose them to the Tauri frontend.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::SampleRate;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;

const TARGET_RATE: u32 = 48_000;

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("nbfm-audio.log")
}

fn log(msg: &str) {
    eprintln!("{msg}");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{msg}");
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub friendly_name: String,
    pub default_sample_rate: u32,
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    pub supports_48k: bool,
    pub is_default: bool,
    pub error: Option<String>,
}

/// Parse `/proc/asound/cards` → map ALSA card-id (e.g. "CODEC") to its
/// human description (e.g. "USB Audio CODEC").
fn alsa_card_descriptions() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/asound/cards") else {
        return map;
    };
    // Line format: " 0 [CODEC          ]: USB-Audio - USB Audio CODEC"
    for line in text.lines() {
        if let Some(open) = line.find('[') {
            if let Some(close) = line[open..].find(']') {
                let card_id = line[open + 1..open + close].trim().to_string();
                if let Some(dash) = line[open + close..].find(" - ") {
                    let desc = line[open + close + dash + 3..].trim().to_string();
                    if !card_id.is_empty() && !desc.is_empty() {
                        map.insert(card_id, desc);
                    }
                }
            }
        }
    }
    map
}

fn friendly(name: &str, cards: &std::collections::HashMap<String, String>) -> String {
    if let Some(rest) = name.strip_prefix("hw:CARD=") {
        let card_id = rest.split(',').next().unwrap_or(rest);
        if let Some(desc) = cards.get(card_id) {
            return format!("{desc} ({card_id})");
        }
    }
    match name {
        "default" => "Système (default ALSA)".into(),
        "pulse" => "PulseAudio".into(),
        "pipewire" => "PipeWire".into(),
        _ => name.to_string(),
    }
}

/// Keep only meaningful entries: one direct-hardware alias per ALSA card
/// (`hw:CARD=*,DEV=0`) plus the high-level `default`/`pulse`/`pipewire`
/// aliases. Drops `sysdefault`, `surround*`, `iec958`, `dsnoop`, `front`,
/// rate converters, etc., which would otherwise flood the dropdown.
/// On non-Linux hosts (WASAPI / CoreAudio), cpal already returns one clean
/// entry per device — no filtering needed.
#[cfg(target_os = "linux")]
fn keep_device(name: &str) -> bool {
    matches!(name, "default" | "pulse" | "pipewire")
        || (name.starts_with("hw:CARD=") && name.ends_with(",DEV=0"))
}

#[cfg(not(target_os = "linux"))]
fn keep_device(_name: &str) -> bool {
    true
}

pub fn list_input_devices() -> Result<Vec<DeviceInfo>, Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(log_path());
    let host = cpal::default_host();
    log(&format!("[audio] host = {:?}", host.id()));
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    log(&format!("[audio] default input device = {default_name:?}"));

    let cards = alsa_card_descriptions();
    let mut out = Vec::new();
    for device in host.input_devices()? {
        let name = match device.name() {
            Ok(n) => n,
            Err(e) => {
                log(&format!("[audio] device.name() failed: {e}"));
                continue;
            }
        };
        if !keep_device(&name) {
            log(&format!("[audio] skip {name}"));
            continue;
        }
        let is_default = default_name.as_deref() == Some(name.as_str());
        let default_sample_rate = device
            .default_input_config()
            .map(|c| c.sample_rate().0)
            .unwrap_or(0);

        let (min_sample_rate, max_sample_rate, supports_48k, error) =
            match device.supported_input_configs() {
                Ok(iter) => {
                    let mut min = u32::MAX;
                    let mut max = 0u32;
                    let mut has_48k = false;
                    for cfg in iter {
                        let lo = cfg.min_sample_rate().0;
                        let hi = cfg.max_sample_rate().0;
                        if lo < min {
                            min = lo;
                        }
                        if hi > max {
                            max = hi;
                        }
                        if (lo..=hi).contains(&TARGET_RATE) {
                            has_48k = true;
                        }
                    }
                    if max == 0 {
                        (0, 0, false, Some("no supported config".into()))
                    } else {
                        (min, max, has_48k, None)
                    }
                }
                Err(e) => (0, 0, false, Some(e.to_string())),
            };

        log(&format!(
            "[audio] {name} default={default_sample_rate} range={min_sample_rate}-{max_sample_rate} 48k={supports_48k} default_dev={is_default} err={error:?}"
        ));

        let friendly_name = friendly(&name, &cards);
        out.push(DeviceInfo {
            name,
            friendly_name,
            default_sample_rate,
            min_sample_rate,
            max_sample_rate,
            supports_48k,
            is_default,
            error,
        });
    }
    Ok(out)
}

pub fn list_output_devices() -> Result<Vec<DeviceInfo>, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let default_name = host.default_output_device().and_then(|d| d.name().ok());
    let cards = alsa_card_descriptions();
    let mut out = Vec::new();
    for device in host.output_devices()? {
        let name = match device.name() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !keep_device(&name) {
            continue;
        }
        let is_default = default_name.as_deref() == Some(name.as_str());
        let default_sample_rate = device
            .default_output_config()
            .map(|c| c.sample_rate().0)
            .unwrap_or(0);
        let (min_sample_rate, max_sample_rate, supports_48k, error) =
            match device.supported_output_configs() {
                Ok(iter) => {
                    let mut min = u32::MAX;
                    let mut max = 0u32;
                    let mut has_48k = false;
                    for cfg in iter {
                        let lo = cfg.min_sample_rate().0;
                        let hi = cfg.max_sample_rate().0;
                        if lo < min {
                            min = lo;
                        }
                        if hi > max {
                            max = hi;
                        }
                        if (lo..=hi).contains(&TARGET_RATE) {
                            has_48k = true;
                        }
                    }
                    if max == 0 {
                        (0, 0, false, Some("no supported config".into()))
                    } else {
                        (min, max, has_48k, None)
                    }
                }
                Err(e) => (0, 0, false, Some(e.to_string())),
            };
        let friendly_name = friendly(&name, &cards);
        out.push(DeviceInfo {
            name,
            friendly_name,
            default_sample_rate,
            min_sample_rate,
            max_sample_rate,
            supports_48k,
            is_default,
            error,
        });
    }
    Ok(out)
}

/// Verify that a named device can be opened at 48 kHz; returns the actual
/// sample-rate that will be used (target if accepted, else device default).
#[allow(dead_code)]
pub fn check_device_48k(name: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host
        .input_devices()?
        .find(|d| d.name().map(|n| n == name).unwrap_or(false))
        .ok_or("device not found")?;
    for cfg in device.supported_input_configs()? {
        if cfg.min_sample_rate().0 <= TARGET_RATE && TARGET_RATE <= cfg.max_sample_rate().0 {
            let _ = cfg.with_sample_rate(SampleRate(TARGET_RATE));
            return Ok(TARGET_RATE);
        }
    }
    Ok(device.default_input_config()?.sample_rate().0)
}
