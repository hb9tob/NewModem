use crate::overlay::{default_overlay_slots, Overlay};
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
    /// If true, the TX worker writes the synthesised audio to a WAV
    /// (mono, 48 kHz, int16) under `<save_dir>/tx_history/` next to the
    /// archived source. The legacy in-process pipeline dropped the
    /// intermediate WAV; this toggle reinstates it for users who want
    /// to inspect / replay the on-air signal offline. Default false —
    /// keeps disk usage bounded for kiosk operators.
    #[serde(default)]
    pub tx_save_wav: bool,
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
    /// Five fixed overlay slots. Slot 0 is the immutable "Aucun" entry
    /// (no overlay applied); slots 1..=4 are user-editable templates
    /// for callsign / club logo / etc. Baked into the resized image
    /// inside `compress_avif` so the preview and the transmitted bytes
    /// match.
    #[serde(default = "default_overlay_slots")]
    pub overlays: Vec<Overlay>,
    /// Index of the currently active slot (0..=4). 0 means no overlay.
    #[serde(default)]
    pub active_overlay: u32,
    /// Set to `true` once the bundled default overlay (logo on slot 1)
    /// has been written to disk and seeded into the settings. Stays
    /// `false` on legacy `settings.json` files so the first run after
    /// an upgrade installs the default overlay even when the rest of
    /// the user's preferences are preserved.
    #[serde(default)]
    pub overlay_default_seeded: bool,

    // ───── PlutoSDR controls ─────────────────────────────────────────
    //
    // RX and TX have separate LO frequencies on the AD9361 (split-band
    // operation is common on amateur duplex repeaters: RX one freq,
    // TX a paired offset). The GUI exposes both independently. TX
    // power on Pluto is `hardwaregain` on the TX baseband channel,
    // expressed as attenuation in dB (positive = more attenuation).
    /// Pluto RX_LO frequency in Hz. Default 145.5 MHz (2 m ham band).
    #[serde(default = "default_pluto_rx_freq_hz")]
    pub pluto_rx_freq_hz: u64,
    /// Pluto TX_LO frequency in Hz. Default 145.5 MHz (= RX, simplex).
    /// Set independently for repeater duplex (e.g. RX 145.625 / TX
    /// 145.025 with a 600 kHz negative shift).
    #[serde(default = "default_pluto_tx_freq_hz")]
    pub pluto_tx_freq_hz: u64,
    /// Pluto RX gain control mode. One of the AD9361 strings
    /// `manual` / `fast_attack` / `slow_attack` / `hybrid` (the
    /// chip's `gain_control_mode_available` set). Default
    /// `slow_attack` — what the AD9361 driver itself defaults to,
    /// nice smooth tracking for ham-band receive. Switch to
    /// `manual` and tune `pluto_rx_gain_db` for lab work where
    /// the input level is known.
    #[serde(default = "default_pluto_rx_gain_mode")]
    pub pluto_rx_gain_mode: String,
    /// Pluto RX manual gain in dB. Range `[-3, 71]` step 1 dB on
    /// the AD9363. Default 30. Only honoured when
    /// `pluto_rx_gain_mode = "manual"` — in any AGC mode the chip
    /// picks gain on its own.
    #[serde(default = "default_pluto_rx_gain_db")]
    pub pluto_rx_gain_db: i32,
    /// Pluto TX attenuation in dB (positive = more attenuation, less
    /// output power). Range `[0, 89.75]` step 0.25 dB. Default 30
    /// (a safe medium-power value — full power at 0 can saturate
    /// nearby receivers without an antenna isolator).
    #[serde(default = "default_pluto_tx_attenuation_db")]
    pub pluto_tx_attenuation_db: f64,
    /// Pluto RX FM max deviation in Hz. `5000` = NBFM standard,
    /// `2500` = narrow NFM (some repeaters / PMR-style channels).
    /// The discriminator gain scales as `1 / max_deviation`; picking
    /// it below the actual on-air deviation soft-clips the audio,
    /// above attenuates it. Default 5000.
    #[serde(default = "default_pluto_rx_deviation_hz")]
    pub pluto_rx_deviation_hz: u32,
    /// Pluto TX FM deviation preset in Hz, before the fine-tune
    /// offset. `5000` = NBFM, `2500` = narrow NFM. The effective
    /// deviation passed to the modulator is
    /// [`Self::effective_pluto_tx_deviation_hz`] = preset + offset.
    /// Default 5000.
    #[serde(default = "default_pluto_tx_deviation_preset_hz")]
    pub pluto_tx_deviation_preset_hz: u32,
    /// Fine-tune offset around the TX deviation preset, in Hz.
    /// Range `[-2000, +2000]`. Lets the operator dial in a value
    /// that the local repeater accepts without clipping (some clip
    /// at ~4.2 kHz, others tolerate up to 6 kHz). Default 0.
    #[serde(default = "default_pluto_tx_deviation_offset_hz")]
    pub pluto_tx_deviation_offset_hz: i32,
    /// Whether the CTCSS sub-audible tone is mixed into the TX
    /// audio. Off by default — most simplex contacts and many
    /// repeaters don't need it. Enable when transmitting through a
    /// CTCSS-protected repeater.
    #[serde(default)]
    pub pluto_tx_ctcss_enabled: bool,
    /// CTCSS tone frequency in Hz. One of the 39 EIA standard
    /// values (67.0 – 254.1 Hz). Default 88.5 Hz, the most common
    /// French / Swiss 2 m repeater pilot.
    #[serde(default = "default_pluto_tx_ctcss_freq_hz")]
    pub pluto_tx_ctcss_freq_hz: f32,
    /// MRU list of recently-used Pluto frequencies, in Hz. Auto-fed
    /// from the on-screen frequency keypad on every Valider press;
    /// the GUI surfaces the list as a row of quick-pick buttons at
    /// the top of the keypad. Capped at 6 entries (most recent
    /// first); deduplicated on insert. Empty on first run.
    #[serde(default)]
    pub pluto_freq_favorites: Vec<u64>,

    // ───── SDRplay RSPduo controls ───────────────────────────────────
    //
    // RX-only — RSPduo is RX-only hardware. Kept in the same
    // settings.json so the GUI can switch between Pluto and SDRplay
    // without losing state on either side. Backend `start_capture`
    // routes on the device-name prefix (`sdrplay:` vs `pluto:`).
    /// Tuner half of the RSPduo to use in single-tuner mode.
    /// `"A"` (Tuner 1, has both Hi-Z and 50 Ω ports) or `"B"`
    /// (Tuner 2, single 50 Ω port — and the only one with a
    /// bias-T). Default `"B"` because the bias-T-on-port-B is
    /// where most external preamps end up.
    #[serde(default = "default_sdrplay_tuner")]
    pub sdrplay_tuner: String,
    /// Antenna port on Tuner A. `"hiz"` = AMPORT_1 (1 kHz-60 MHz),
    /// `"fifty"` = AMPORT_2 (60 MHz-2 GHz). Ignored when
    /// `sdrplay_tuner = "B"`. Default `"fifty"` for VHF amateur.
    #[serde(default = "default_sdrplay_antenna")]
    pub sdrplay_antenna: String,
    /// **Bias-T on Tuner B's port.** Pushes +5 V DC up the
    /// antenna cable to power external preamps. RSPduo only
    /// exposes bias-T on Tuner B; turning it on while
    /// `sdrplay_tuner = "A"` has no effect on the active path.
    /// Default false — leaving DC on the cable when nothing
    /// expects it can damage some receivers.
    #[serde(default)]
    pub sdrplay_bias_t: bool,
    /// Broadcast-FM rejection notch (~88-108 MHz). Default false.
    #[serde(default)]
    pub sdrplay_fm_notch: bool,
    /// DAB band-III rejection notch (~174-240 MHz). Default false.
    #[serde(default)]
    pub sdrplay_dab_notch: bool,
    /// RSPduo RX LO frequency in Hz. Default 145.5 MHz.
    #[serde(default = "default_sdrplay_rx_freq_hz")]
    pub sdrplay_rx_freq_hz: u64,
    /// LNA state index. RSPduo's VHF table has 10 states,
    /// 0 = least attenuation = most gain. Default 4 (mid-band).
    #[serde(default = "default_sdrplay_lna_state")]
    pub sdrplay_lna_state: u8,
    /// IF gain reduction in dB. Range 20-59 (the chip's
    /// `gRdB`). Ignored when `sdrplay_agc_mode != "disable"`
    /// (the daemon manages it). Default 40.
    #[serde(default = "default_sdrplay_if_gain_reduction_db")]
    pub sdrplay_if_gain_reduction_db: i32,
    /// AGC loop bandwidth, mapped onto `sdrplay_api_AgcControlT`:
    /// `"disable"` → manual gain only (default), `"slow"` = 5 Hz,
    /// `"mid"` = 50 Hz (the SDRplay default), `"fast"` = 100 Hz.
    /// LNA state stays manual whatever the AGC mode.
    #[serde(default = "default_sdrplay_agc_mode")]
    pub sdrplay_agc_mode: String,
    /// Maximum FM deviation expected on air (Hz). 5000 = NBFM
    /// standard, 2500 = narrow NFM. Drives the QuadratureDemod
    /// gain in the DSP chain.
    #[serde(default = "default_sdrplay_rx_deviation_hz")]
    pub sdrplay_rx_deviation_hz: u32,
    /// MRU list of recently-used SDRplay frequencies (mirror of
    /// `pluto_freq_favorites` for the SDRplay-tab freq keypad).
    #[serde(default)]
    pub sdrplay_freq_favorites: Vec<u64>,
}

impl Settings {
    /// Effective TX deviation in Hz = preset + fine-tune offset,
    /// clamped to a sane range (500 Hz min, 8 kHz max). Always use
    /// this accessor when feeding `PlutoConfig::tx_deviation_hz`;
    /// callers must NOT add preset+offset themselves to keep the
    /// clamp policy in one place.
    pub fn effective_pluto_tx_deviation_hz(&self) -> u32 {
        let raw = self.pluto_tx_deviation_preset_hz as i32 + self.pluto_tx_deviation_offset_hz;
        raw.clamp(500, 8000) as u32
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

fn default_pluto_rx_freq_hz() -> u64 {
    145_500_000
}

fn default_pluto_tx_freq_hz() -> u64 {
    145_500_000
}

fn default_pluto_rx_gain_mode() -> String {
    "slow_attack".to_string()
}

fn default_pluto_rx_gain_db() -> i32 {
    30
}

fn default_pluto_tx_attenuation_db() -> f64 {
    30.0
}

fn default_pluto_rx_deviation_hz() -> u32 {
    5000
}

fn default_pluto_tx_deviation_preset_hz() -> u32 {
    5000
}

fn default_pluto_tx_deviation_offset_hz() -> i32 {
    0
}

fn default_pluto_tx_ctcss_freq_hz() -> f32 {
    88.5
}

fn default_sdrplay_tuner() -> String {
    "B".to_string()
}
fn default_sdrplay_antenna() -> String {
    "fifty".to_string()
}
fn default_sdrplay_rx_freq_hz() -> u64 {
    145_500_000
}
fn default_sdrplay_lna_state() -> u8 {
    4
}
fn default_sdrplay_if_gain_reduction_db() -> i32 {
    40
}
fn default_sdrplay_agc_mode() -> String {
    "disable".to_string()
}
fn default_sdrplay_rx_deviation_hz() -> u32 {
    5000
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
            tx_save_wav: false,
            rx_force_mode: false,
            rx_forced_profile: default_rx_forced_profile(),
            experimental_modes_enabled: false,
            overlays: default_overlay_slots(),
            active_overlay: 0,
            overlay_default_seeded: false,
            pluto_rx_freq_hz: default_pluto_rx_freq_hz(),
            pluto_tx_freq_hz: default_pluto_tx_freq_hz(),
            pluto_rx_gain_mode: default_pluto_rx_gain_mode(),
            pluto_rx_gain_db: default_pluto_rx_gain_db(),
            pluto_tx_attenuation_db: default_pluto_tx_attenuation_db(),
            pluto_rx_deviation_hz: default_pluto_rx_deviation_hz(),
            pluto_tx_deviation_preset_hz: default_pluto_tx_deviation_preset_hz(),
            pluto_tx_deviation_offset_hz: default_pluto_tx_deviation_offset_hz(),
            pluto_tx_ctcss_enabled: false,
            pluto_tx_ctcss_freq_hz: default_pluto_tx_ctcss_freq_hz(),
            pluto_freq_favorites: Vec::new(),
            sdrplay_tuner: default_sdrplay_tuner(),
            sdrplay_antenna: default_sdrplay_antenna(),
            sdrplay_bias_t: false,
            sdrplay_fm_notch: false,
            sdrplay_dab_notch: false,
            sdrplay_rx_freq_hz: default_sdrplay_rx_freq_hz(),
            sdrplay_lna_state: default_sdrplay_lna_state(),
            sdrplay_if_gain_reduction_db: default_sdrplay_if_gain_reduction_db(),
            sdrplay_agc_mode: default_sdrplay_agc_mode(),
            sdrplay_rx_deviation_hz: default_sdrplay_rx_deviation_hz(),
            sdrplay_freq_favorites: Vec::new(),
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
