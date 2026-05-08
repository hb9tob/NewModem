//! Backend-agnostic runtime config — what the GUI hands to a
//! backend's `open()`.
//!
//! The fields are the "greatest common denominator" across the
//! NBFM SDRs we support today (Pluto + SDRplay) plus the ones we
//! plan to add (RTL-SDR, Lime). Anything that doesn't fit here
//! goes into [`SdrConfig::backend_extras`] as `serde_json::Value`,
//! and the backend's module-level docs list which keys it
//! recognizes (see `modem-pluto::backend` and `modem-sdrplay::backend`).
//!
//! The GUI persists this struct (one per backend ID) inside
//! `Settings::sdr_settings.backends[backend_id].config` — not as
//! scattered top-level fields. That way adding a backend = one
//! new entry in the HashMap, never a settings.json schema bump.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SdrConfig {
    /// `SdrBackend::id()` of the producing backend. Persisted so the
    /// GUI can tell which `default_sdr_config_for(...)` to seed when
    /// the field is missing on first run.
    pub backend_id: String,
    /// Opaque device ID the backend's `open()` understands —
    /// libiio URI for Pluto, serial for SDRplay.
    pub device_id: String,

    pub rx_freq_hz: u64,
    /// Equals `rx_freq_hz` on RX-only or simplex backends. Pluto-style
    /// duplex (independent TX_LO) uses a separate value.
    pub tx_freq_hz: u64,

    pub gain: GainSetting,

    /// Drives `QuadratureDemod` discriminator gain on RX. NBFM
    /// standard = 5000 Hz, narrow NFM = 2500 Hz. Backend-agnostic:
    /// every NBFM front-end consumes this the same way.
    pub max_deviation_hz: f32,
    /// Drives `PhaseMod::k_p` on TX. Equal to `max_deviation_hz`
    /// on RX-only backends (the field is ignored there). On Pluto
    /// it can differ: e.g. RX scaled for 5 kHz reception, TX
    /// scaled for narrow-NFM transmission.
    pub tx_deviation_hz: f32,

    /// Selected antenna ID — must match one of
    /// `BackendCapabilities::antennas[].id`. Empty `""` when the
    /// backend has no antenna selector.
    pub antenna: String,

    pub bias_t: bool,
    pub fm_notch: bool,
    pub dab_notch: bool,

    /// CTCSS sub-audible tone for repeater squelch — TX-only.
    /// `0.0` = disabled. Only meaningful on backends with
    /// `BackendFeatures::ctcss_tx = true`.
    pub ctcss_freq_hz: f32,
    /// CTCSS tone level relative to ±max_deviation_hz peak.
    /// Typical = 0.1 (≈ ±500 Hz on a 5 kHz NBFM signal).
    pub ctcss_level: f32,

    /// RF-bandwidth in Hz, when
    /// `BackendFeatures::rf_bandwidth_range_hz` is `Some`. `None`
    /// = backend default (Pluto: 200 kHz; SDRplay: locked at
    /// 1.536 MHz).
    pub rf_bandwidth_hz: Option<u64>,

    /// Escape hatch for backend-specific knobs that don't justify
    /// a typed top-level field. Example payload for Pluto:
    /// `{"tx_attenuation_db": 30.0, "prefer_low_rate": true}`.
    /// Example for SDRplay: `{"tuner": "B", "decimation": 4}`.
    /// Each backend's docs list its keys; unknown keys are
    /// silently ignored.
    #[serde(default)]
    pub backend_extras: HashMap<String, serde_json::Value>,
}

impl Default for SdrConfig {
    fn default() -> Self {
        Self {
            backend_id: String::new(),
            device_id: String::new(),
            rx_freq_hz: 0,
            tx_freq_hz: 0,
            gain: GainSetting::default(),
            max_deviation_hz: 5_000.0, // NBFM standard
            tx_deviation_hz: 5_000.0,
            antenna: String::new(),
            bias_t: false,
            fm_notch: false,
            dab_notch: false,
            ctcss_freq_hz: 0.0,
            ctcss_level: 0.1,
            rf_bandwidth_hz: None,
            backend_extras: HashMap::new(),
        }
    }
}

/// What the GUI persists as the gain setpoint. The backend's
/// `open()` cross-references against
/// `BackendCapabilities::manual_gain` and `agc_modes` to validate
/// the variant matches the shape it advertised.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GainSetting {
    /// Manual gain, in the shape advertised by
    /// `ManualGainShape`.
    Manual(ManualGainValue),
    /// Backend-named AGC mode. `id` must match one of
    /// `BackendCapabilities::agc_modes[].id`.
    AgcMode { id: String },
}

impl Default for GainSetting {
    fn default() -> Self {
        // Neutral default; the GUI always replaces via
        // `default_sdr_config_for(backend_id)` before persisting.
        Self::Manual(ManualGainValue::Db { db: 0 })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum ManualGainValue {
    /// Pluto-style continuous dB.
    Db { db: i32 },
    /// SDRplay-style LNA state index + IF gain reduction in dB.
    LnaPlusIf { lna_state: u8, if_grdb: i32 },
    /// Index into the discrete dB ladder.
    Discrete { step_idx: usize },
}

impl Default for ManualGainValue {
    fn default() -> Self {
        Self::Db { db: 0 }
    }
}
