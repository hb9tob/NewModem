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
    /// RTS line level when transmitting (true = high). Default true: the
    /// most common convention on commercial radio-amateur interfaces.
    #[serde(default = "default_true")]
    pub ptt_rts_tx_high: bool,
    #[serde(default = "default_true")]
    pub ptt_dtr_tx_high: bool,
    /// Attenuation applied to the TX WAV before sending it to the sound
    /// card, in dB (<= 0). Filled in by the Channel tab (ATT cascade).
    /// Applied gain: `10^(att/20)`. Default: 0 dB (no attenuation).
    pub tx_attenuation_db: f32,
    /// If true, applies an NBFM pre-emphasis +6 dB/octave shelf
    /// (tau1 = 750 us / tau2 = 75 us, breakpoints ~212 Hz and ~2.12 kHz)
    /// to the WAV before sending it to the sound card. Useful to
    /// compensate a TX audio chain that de-emphasizes too aggressively,
    /// or to recover the slope expected by a transceiver whose internal
    /// pre-emphasis is absent. The filtered signal is re-normalized to
    /// peak 0.9 to avoid sound-card clipping. Default: false.
    #[serde(default)]
    pub tx_preemphasis_enabled: bool,
    /// If true, applies an NBFM de-emphasis -6 dB/octave shelf
    /// (mathematical inverse of `tx_preemphasis_enabled`, same break
    /// points ~212 Hz / ~2.12 kHz, plateau -20 dB) to the captured
    /// audio on the path going to the modem demodulator. The raw WAV
    /// capture and the audio level meter stay on the unfiltered signal.
    /// Useful when the receiving transceiver does not apply enough
    /// de-emphasis (or none) and the high-frequency boost from the
    /// transmitter side hurts EVM. Default: false.
    #[serde(default)]
    pub rx_deemphasis_enabled: bool,
    /// Base URL of the Phase-D collector (e.g. `https://hb9tob-modem.duckdns.org`).
    /// If empty, the post-raw-capture prompt does not appear and submission
    /// is disabled for the session.
    pub collector_url: String,
    /// AVIF quality remembered across sessions (0-100). Default 10:
    /// compact file for slow NBFM passes.
    #[serde(default = "default_tx_quality")]
    pub tx_quality: u32,
    /// Percentage of RaptorQ repair blocks added to the initial burst
    /// (0, 5, 10, 20, ...). Default 5: modest redundancy, the user can
    /// raise it as needed.
    #[serde(default = "default_tx_repair_pct")]
    pub tx_repair_pct: u32,
    /// Modem mode selected in the TX panel. Standard profiles:
    /// ULTRA / ROBUST / NORMAL / HIGH / HIGH56 / HIGH+. Experimental
    /// profiles (visible only when `experimental_modes_enabled`):
    /// MEGA / FAST / HIGH++ / HIGH+56. Default HIGH56.
    #[serde(default = "default_tx_mode")]
    pub tx_mode: String,
    /// Resize choice (`none`, `1920x1024`, `800x600`, `free`).
    #[serde(default = "default_tx_resize")]
    pub tx_resize: String,
    /// Dimensions entered in `free` mode.
    #[serde(default = "default_tx_free_w")]
    pub tx_free_w: u32,
    #[serde(default = "default_tx_free_h")]
    pub tx_free_h: u32,
    /// AVIF encoder speed, 1..=10.
    #[serde(default = "default_tx_speed")]
    pub tx_speed: u32,
    /// Number of additional blocks for TX more (1..).
    #[serde(default = "default_tx_more_count")]
    pub tx_more_count: u32,
    /// Maximum size of the TX history (number of files retained in
    /// `<save_dir>/tx_history/`). Default 100. Beyond that, older files
    /// are purged on each archive.
    #[serde(default = "default_tx_history_max")]
    pub tx_history_max: u32,
    /// Lock the RX onto a specific profile (bypassing the FFT-gate
    /// auto-detection). Required to decode experimental profiles
    /// (MEGA, FAST, HIGH++, HIGH+56) that are absent from
    /// `PROBE_TEMPLATES`. Default false.
    #[serde(default)]
    pub rx_force_mode: bool,
    /// Profile to force on the RX side when `rx_force_mode = true`.
    /// Ignored otherwise. Default HIGH56 (the recommended standard
    /// profile).
    #[serde(default = "default_rx_forced_profile")]
    pub rx_forced_profile: String,
    /// Show/hide experimental profiles in the TX and RX combos plus
    /// the "Force a profile" option on RX startup. Default false: the
    /// user discovers the app with only standard profiles exposed.
    /// Toggleable from the Settings tab.
    #[serde(default)]
    pub experimental_modes_enabled: bool,
}

fn default_tx_quality() -> u32 {
    10
}

fn default_tx_repair_pct() -> u32 {
    5
}

fn default_tx_mode() -> String {
    "HIGH56".to_string()
}

fn default_rx_forced_profile() -> String {
    "HIGH56".to_string()
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

fn default_tx_history_max() -> u32 {
    100
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
            tx_preemphasis_enabled: false,
            rx_deemphasis_enabled: false,
            collector_url: String::new(),
            tx_quality: default_tx_quality(),
            tx_repair_pct: default_tx_repair_pct(),
            tx_mode: default_tx_mode(),
            tx_resize: default_tx_resize(),
            tx_free_w: default_tx_free_w(),
            tx_free_h: default_tx_free_h(),
            tx_speed: default_tx_speed(),
            tx_more_count: default_tx_more_count(),
            tx_history_max: default_tx_history_max(),
            rx_force_mode: false,
            rx_forced_profile: default_rx_forced_profile(),
            experimental_modes_enabled: false,
        }
    }
}

/// Portable mode: if a `portable.txt` marker file sits next to the GUI
/// executable, all state (settings, RX captures, sessions) is confined
/// to `<exe_dir>/data/`. Otherwise `None` and we fall back to standard
/// OS paths (`%APPDATA%`, `~/Downloads`).
pub fn portable_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if dir.join("portable.txt").exists() {
        Some(dir.join("data"))
    } else {
        None
    }
}

fn settings_path() -> PathBuf {
    if let Some(root) = portable_root() {
        return root.join("settings.json");
    }
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
