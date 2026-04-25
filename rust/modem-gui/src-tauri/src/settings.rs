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
    /// URL de base du collector Phase D (ex: `https://hb9tob-modem.duckdns.org`).
    /// Si vide, le prompt post-capture brute n'apparaît pas et la
    /// soumission est désactivée pour la session.
    pub collector_url: String,
    /// Qualité AVIF mémorisée entre sessions (0-100). Défaut 10 :
    /// fichier compact pour passe NBFM lente.
    #[serde(default = "default_tx_quality")]
    pub tx_quality: u32,
    /// % de blocs repair RaptorQ ajoutés au burst initial (0, 5, 10, 20...).
    /// Défaut 5 : redondance modeste, l'utilisateur monte au besoin.
    #[serde(default = "default_tx_repair_pct")]
    pub tx_repair_pct: u32,
    /// Mode modem sélectionné dans la fenêtre TX (ULTRA / ROBUST / NORMAL /
    /// HIGH / MEGA). Défaut HIGH (correspond à l'option HTML cochée).
    #[serde(default = "default_tx_mode")]
    pub tx_mode: String,
    /// Choix de redimensionnement (`none`, `1920x1024`, `800x600`, `free`).
    #[serde(default = "default_tx_resize")]
    pub tx_resize: String,
    /// Dimensions saisies en mode `free`.
    #[serde(default = "default_tx_free_w")]
    pub tx_free_w: u32,
    #[serde(default = "default_tx_free_h")]
    pub tx_free_h: u32,
    /// Vitesse encodeur AVIF, 1..=10.
    #[serde(default = "default_tx_speed")]
    pub tx_speed: u32,
    /// Nombre de blocs additionnels pour TX more (1..).
    #[serde(default = "default_tx_more_count")]
    pub tx_more_count: u32,
}

fn default_tx_quality() -> u32 {
    10
}

fn default_tx_repair_pct() -> u32 {
    5
}

fn default_tx_mode() -> String {
    "HIGH".to_string()
}

fn default_tx_resize() -> String {
    "800x600".to_string()
}

fn default_tx_free_w() -> u32 {
    800
}

fn default_tx_free_h() -> u32 {
    600
}

fn default_tx_speed() -> u32 {
    6
}

fn default_tx_more_count() -> u32 {
    5
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
            collector_url: String::new(),
            tx_quality: default_tx_quality(),
            tx_repair_pct: default_tx_repair_pct(),
            tx_mode: default_tx_mode(),
            tx_resize: default_tx_resize(),
            tx_free_w: default_tx_free_w(),
            tx_free_h: default_tx_free_h(),
            tx_speed: default_tx_speed(),
            tx_more_count: default_tx_more_count(),
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
