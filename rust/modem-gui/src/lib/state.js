// Shared layer — the single mutable app-state store.
// `currentSettings` / `modemProfiles` are reassigned only through the setters
// here (ESM imported bindings are read-only in other modules), while every tab
// reads them live and mutates their PROPERTIES (currentSettings.x = y) freely.
// Imports nothing.

export let currentSettings = {
  callsign: "",
  rx_device: "",
  tx_device: "",
  ptt_enabled: false,
  ptt_port: "",
  ptt_use_rts: true,
  ptt_use_dtr: false,
  ptt_rts_tx_high: true,
  ptt_dtr_tx_high: true,
  tx_attenuation_db: 0,
  tx_preemphasis_enabled: false,
  tx_save_wav: false,
  rx_deemphasis_enabled: false,
  rx_allow_legacy_grid: true,
  /// When true the RX worker forks to the fully-streaming TURBO decode
  /// path (`rx_v3_worker`) with profile auto-detection, instead of the
  /// legacy sliding-window `rx_v2`. Independent of `rx_allow_legacy_grid`
  /// (Power Mode) and of `rx_force_mode`. Drives the dark-red background.
  rx_turbo: false,
  audio_backend: "alsa",
  collector_url: "",
  tx_quality: 10,
  tx_repair_pct: 5,
  tx_mode: "HIGH56",
  tx_resize: "800x600",
  tx_free_w: 800,
  tx_free_h: 600,
  tx_speed: 6,
  tx_more_count: 5,
  /// If true, the RX profile is locked on rx_forced_profile and auto-
  /// detection is disabled. Required to receive FAST, HIGH++ or
  /// HIGH+56 (which are outside PROBE_TEMPLATES).
  rx_force_mode: false,
  rx_forced_profile: "HIGH56",
  /// If true, experimental profiles appear in the TX and RX combos and
  /// the "Force a profile" UI becomes visible at startup. If false
  /// (default): combos are filtered to standard profiles (ULTRA /
  /// ROBUST / NORMAL / HIGH / HIGH56 / HIGH+) and the rx-force-bar is
  /// hidden.
  experimental_modes_enabled: false,
  /// If true, TX no longer stops RX before transmitting and a dedicated
  /// TX progress bar (violet → logo blue) appears above the RX bar
  /// while both run concurrently. Default false: keep the historical
  /// half-duplex behaviour (txStart stops RX, maybeRestartRx restarts it
  /// after tx_complete). Mirror of the Rust-side `full_duplex_enabled`.
  full_duplex_enabled: false,
  overlays: [],
  active_overlay: 0,
  // Radio-tab monitoring controls (mirror of the Rust RadioUiSettings).
  radio: {
    squelch_enabled: false,
    squelch_dbfs: -80,
    monitor_device: null,
    monitor_volume: 0.80,
    smeter_cal_trim_db: 0,
  },
};

export let modemProfiles = [];

// Reassign the store bindings. Other modules import `currentSettings` /
// `modemProfiles` as live read-only bindings and must call these to REPLACE the
// object (property mutation `currentSettings.x = y` stays direct everywhere).
export function setSettings(s) {
  currentSettings = s;
}

export function setModemProfiles(p) {
  modemProfiles = p;
}
