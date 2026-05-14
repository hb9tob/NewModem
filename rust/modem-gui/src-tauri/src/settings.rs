use crate::overlay::{default_overlay_slots, Overlay};
use modem_sdr::{GainSetting, ManualGainValue, SdrConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Bumped every time the SDR-related portion of `Settings` changes
/// shape. On load, values < `SETTINGS_SCHEMA_VERSION` trigger a
/// targeted reset of the SDR fields (`rx_device`, `tx_device`,
/// `sdr_settings`); all other fields (callsign, PTT, overlays,
/// preprocessing toggles, …) are preserved.
///
/// History:
///   1 → settings.json before the SDR-agnostic refactor (35 scattered
///       `pluto_*` / `sdrplay_*` fields). Reading such a file dumps
///       those fields into the new SdrSettings via reset.
///   2 → SDR-agnostic schema (this release): `sdr_settings:
///       SdrSettings` with one entry per backend.
pub const SETTINGS_SCHEMA_VERSION: u32 = 2;

fn default_true() -> bool {
    true
}

/// Default base URL of the Phase-D collector. Used both for the
/// `Default for Settings` factory and as a soft migration target when
/// loading an old settings file with an empty `collector_url`.
pub const DEFAULT_COLLECTOR_URL: &str = "https://hb9tob.duckdns.org";

fn default_collector_url() -> String {
    DEFAULT_COLLECTOR_URL.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Settings-file schema version. New fields default to 0 on
    /// missing → triggers a one-time SDR reset on first run after
    /// upgrade. See [`SETTINGS_SCHEMA_VERSION`].
    pub settings_schema_version: u32,

    pub callsign: String,
    /// Composite device name produced by `SdrBackend::list_devices`,
    /// e.g. `"pluto:usb:1.6.5"`, `"sdrplay:22340A2A34"`, or a plain
    /// cpal name like `"USB Audio (hw:1,0)"`. The registry's
    /// `parse_composite_name` distinguishes the two.
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
    /// card / SDR sink, in dB (<= 0). Filled in by the Channel tab (ATT
    /// cascade). Applied gain: `10^(att/20)`. Default: 0 dB. Worker-level
    /// (not SDR-specific) — applied uniformly to every TX path.
    pub tx_attenuation_db: f32,
    /// If true, applies an NBFM pre-emphasis +6 dB/octave shelf
    /// (tau1 = 750 us / tau2 = 75 us) to the WAV before sending it to
    /// the sink. Worker-level — see `tx_worker::run_playback`.
    #[serde(default)]
    pub tx_preemphasis_enabled: bool,
    /// If true, applies an NBFM de-emphasis -6 dB/octave shelf to the
    /// captured audio on the path going to the modem demodulator.
    /// Worker-level — see `rx_worker::spawn`.
    #[serde(default)]
    pub rx_deemphasis_enabled: bool,

    /// Base URL of the Phase-D collector. Pre-filled with
    /// [`DEFAULT_COLLECTOR_URL`] so out-of-the-box installs talk to the
    /// shared aggregator at `hb9tob.duckdns.org`. The user can override
    /// in Paramètres → Collecteur to point at a private collector;
    /// setting the field empty disables submission for that session.
    #[serde(default = "default_collector_url")]
    pub collector_url: String,
    /// AVIF quality remembered across sessions (0-100). Default 10:
    /// compact file for slow NBFM passes.
    #[serde(default = "default_tx_quality")]
    pub tx_quality: u32,
    /// Percentage of RaptorQ repair blocks added to the initial burst.
    #[serde(default = "default_tx_repair_pct")]
    pub tx_repair_pct: u32,
    #[serde(default = "default_tx_mode")]
    pub tx_mode: String,
    #[serde(default = "default_tx_resize")]
    pub tx_resize: String,
    #[serde(default = "default_tx_free_w")]
    pub tx_free_w: u32,
    #[serde(default = "default_tx_free_h")]
    pub tx_free_h: u32,
    #[serde(default = "default_tx_speed")]
    pub tx_speed: u32,
    #[serde(default = "default_tx_more_count")]
    pub tx_more_count: u32,
    #[serde(default = "default_tx_history_max")]
    pub tx_history_max: u32,
    #[serde(default)]
    pub tx_save_wav: bool,
    #[serde(default)]
    pub rx_force_mode: bool,
    #[serde(default = "default_rx_forced_profile")]
    pub rx_forced_profile: String,
    #[serde(default)]
    pub experimental_modes_enabled: bool,
    /// If true, TX no longer stops RX before transmitting and the GUI
    /// surfaces a dedicated TX progress bar above the RX one when both
    /// are active. Off by default — the modem is half-duplex unless the
    /// user opts in (e.g. RX SDRplay + TX cpal on a separate device, or
    /// future Pluto monodevice). Worker layer is already FDX-ready;
    /// this flag only controls the GUI policy that has been forcing
    /// half-duplex via `txStart` → `stop_capture` → `maybeRestartRx`.
    #[serde(default)]
    pub full_duplex_enabled: bool,
    #[serde(default = "default_overlay_slots")]
    pub overlays: Vec<Overlay>,
    #[serde(default)]
    pub active_overlay: u32,
    #[serde(default)]
    pub overlay_default_seeded: bool,

    /// Per-backend SDR config and MRU favourites. Keyed by
    /// `SdrBackend::id()`. Replaces the 35 scattered `pluto_*` /
    /// `sdrplay_*` fields the legacy schema carried.
    #[serde(default)]
    pub sdr_settings: SdrSettings,
}

/// SDR-side persistent state. One entry per registered backend.
/// Empty map = first run after the schema-2 reset; the GUI seeds
/// each entry on demand via [`Settings::sdr_config_for`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SdrSettings {
    #[serde(default)]
    pub backends: HashMap<String, BackendSettings>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendSettings {
    #[serde(default)]
    pub config: SdrConfig,
    /// MRU list of recently-used frequencies, in Hz (most-recent
    /// first, capped at 6, deduplicated on insert). Replaces the
    /// legacy `pluto_freq_favorites` + `sdrplay_freq_favorites`.
    #[serde(default)]
    pub freq_favorites: Vec<u64>,
}

impl Settings {
    /// Build the [`SdrConfig`] to hand to a particular backend. If
    /// `sdr_settings.backends` carries an entry for `backend_id`, that
    /// stored config is the seed; otherwise we synthesize the
    /// historical default for that backend (see
    /// [`default_sdr_config_for`]). In both cases the `device_id`
    /// field is overwritten with the live composite parse so
    /// `<backend>:<device_id>` round-trips losslessly.
    pub fn sdr_config_for(&self, backend_id: &str, device_id: &str) -> SdrConfig {
        let mut cfg = self
            .sdr_settings
            .backends
            .get(backend_id)
            .map(|bs| bs.config.clone())
            .unwrap_or_else(|| default_sdr_config_for(backend_id));
        cfg.backend_id = backend_id.to_string();
        cfg.device_id = device_id.to_string();
        cfg
    }
}

/// Historical defaults for known backends. Keys come from the
/// `SdrBackend::id()` of each compiled-in impl. Unknown IDs get a
/// barely-useful blank config; the GUI's first save replaces it.
pub fn default_sdr_config_for(backend_id: &str) -> SdrConfig {
    match backend_id {
        "pluto" => {
            let mut cfg = SdrConfig {
                backend_id: "pluto".into(),
                device_id: String::new(),
                rx_freq_hz: 145_500_000,
                tx_freq_hz: 145_500_000,
                // The AD9361 driver's own default — smooth tracking
                // for ham-band receive. The user picks "Manual"
                // explicitly when running level-calibrated lab work.
                gain: GainSetting::AgcMode {
                    id: "slow_attack".into(),
                    lna_state: None,
                },
                max_deviation_hz: 5_000.0,
                tx_deviation_hz: 5_000.0,
                rf_bandwidth_hz: Some(200_000),
                ..SdrConfig::default()
            };
            cfg.backend_extras.insert(
                "tx_attenuation_db".into(),
                serde_json::json!(30.0),
            );
            cfg.backend_extras
                .insert("prefer_low_rate".into(), serde_json::json!(true));
            cfg
        }
        "sdrplay" => {
            let mut cfg = SdrConfig {
                backend_id: "sdrplay".into(),
                device_id: String::new(),
                rx_freq_hz: 145_500_000,
                tx_freq_hz: 145_500_000,
                // RSPduo VHF-default — LNA mid-band, IF gRdB at 40,
                // AGC off (manual gain). Maps onto SdrplayConfig
                // (lna_state=4, if_gain_reduction_db=40, AgcMode::Disable).
                gain: GainSetting::Manual(ManualGainValue::LnaPlusIf {
                    lna_state: 4,
                    if_grdb: 40,
                }),
                max_deviation_hz: 5_000.0,
                tx_deviation_hz: 5_000.0,
                antenna: "fifty".into(),
                ..SdrConfig::default()
            };
            cfg.backend_extras
                .insert("tuner".into(), serde_json::json!("B"));
            cfg.backend_extras
                .insert("decimation".into(), serde_json::json!(4));
            cfg
        }
        _ => SdrConfig {
            backend_id: backend_id.to_string(),
            ..SdrConfig::default()
        },
    }
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
            settings_schema_version: SETTINGS_SCHEMA_VERSION,
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
            collector_url: default_collector_url(),
            tx_quality: default_tx_quality(),
            tx_repair_pct: default_tx_repair_pct(),
            tx_mode: default_tx_mode(),
            tx_resize: default_tx_resize(),
            tx_free_w: default_tx_free_w(),
            tx_free_h: default_tx_free_h(),
            tx_speed: default_tx_speed(),
            tx_more_count: default_tx_more_count(),
            tx_history_max: default_tx_history_max(),
            tx_save_wav: false,
            rx_force_mode: false,
            rx_forced_profile: default_rx_forced_profile(),
            experimental_modes_enabled: false,
            full_duplex_enabled: false,
            overlays: default_overlay_slots(),
            active_overlay: 0,
            overlay_default_seeded: false,
            sdr_settings: SdrSettings::default(),
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
    let mut s: Settings = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // Soft migration for existing installs: previous versions defaulted
    // `collector_url` to an empty string, which disabled submission. We
    // now ship a baked-in default pointing at the public aggregator —
    // backfill it once so an in-place upgrade lights up the upload UI.
    if s.collector_url.trim().is_empty() {
        s.collector_url = default_collector_url();
    }
    if s.settings_schema_version < SETTINGS_SCHEMA_VERSION {
        eprintln!(
            "[settings] schema upgrade {} → {}: SDR settings reset to defaults",
            s.settings_schema_version, SETTINGS_SCHEMA_VERSION,
        );
        // Reset only the SDR-touched fields. Everything else
        // (callsign, PTT config, overlays, channel ATT, …) survives
        // the upgrade.
        s.rx_device = String::new();
        s.tx_device = String::new();
        s.sdr_settings = SdrSettings::default();
        s.settings_schema_version = SETTINGS_SCHEMA_VERSION;
        let _ = save(&s);
    }
    s
}

pub fn save(s: &Settings) -> Result<(), String> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}
