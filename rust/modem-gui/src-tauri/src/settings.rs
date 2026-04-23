use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub callsign: String,
    pub rx_device: String,
    pub tx_device: String,
    pub ptt_enabled: bool,
    pub ptt_port: String,
    #[serde(default = "default_true")]
    pub ptt_use_rts: bool,
    pub ptt_use_dtr: bool,
    /// Niveau RTS quand on émet (true = haut). Défaut true : convention
    /// la plus répandue sur les interfaces commerciales radioamateur.
    #[serde(default = "default_true")]
    pub ptt_rts_tx_high: bool,
    #[serde(default = "default_true")]
    pub ptt_dtr_tx_high: bool,
    /// Atténuation appliquée au WAV TX avant envoi à la carte son, en dB
    /// (≤ 0). Renseignée par l'onglet Canal (cascade ATT). Gain appliqué :
    /// `10^(att/20)`. Défaut : 0 dB (pas d'atténuation).
    pub tx_attenuation_db: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            callsign: String::new(),
            rx_device: String::new(),
            tx_device: String::new(),
            ptt_enabled: false,
            ptt_port: String::new(),
            ptt_use_rts: true,
            ptt_use_dtr: false,
            ptt_rts_tx_high: true,
            ptt_dtr_tx_high: true,
            tx_attenuation_db: 0.0,
        }
    }
}

fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nbfm-modem-gui")
        .join("settings.json")
}

pub fn load() -> Settings {
    let path = settings_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) -> Result<(), String> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}
