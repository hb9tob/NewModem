//! What an SDR backend can do — built by each backend's
//! `capabilities()` and shipped to the GUI as JSON via Tauri.
//!
//! The point of this module: every UI control the GUI used to
//! hardcode (frequency input min/max, AGC `<select>` options,
//! antenna dropdown, bias-T checkbox visibility, gain-row layout)
//! is now expressed here as data. The frontend reads
//! [`BackendCapabilities`] and constructs the panel rows that match.
//!
//! Adding a new backend (RTL-SDR, Lime, …) means:
//!   1. Implement [`crate::SdrBackend`].
//!   2. Return a populated [`BackendCapabilities`].
//!   3. Add it to `modem-gui/src-tauri/src/sdr_registry.rs` behind a
//!      cargo feature.
//! No GUI changes, no HTML edits, no JS edits.

use serde::Serialize;

/// Static capabilities of an SDR backend family. Doesn't reflect
/// per-instance specifics (a particular antenna's actual frequency
/// lock may be tighter than the family's claim) — those are the
/// backend's job to enforce at `open()` time via `SdrError::InvalidConfig`.
#[derive(Clone, Debug, Serialize)]
pub struct BackendCapabilities {
    pub rx_supported: bool,
    pub tx_supported: bool,

    /// RX_LO range, in Hz. `None` when `rx_supported = false`.
    pub rx_freq_range_hz: Option<(u64, u64)>,
    /// TX_LO range, in Hz. `None` when `tx_supported = false`.
    /// May equal `rx_freq_range_hz` on full-duplex backends.
    pub tx_freq_range_hz: Option<(u64, u64)>,

    /// Whether RX_LO and TX_LO can be programmed independently.
    /// `false` for cheap RX-only backends, `true` for Pluto / Lime.
    pub independent_rx_tx_freq: bool,

    /// Manual-gain ergonomics — continuous dB, LNA-state + IF, or a
    /// fixed dB ladder. The frontend uses this to pick the right
    /// widget shape (slider vs two number inputs vs `<select>`).
    pub manual_gain: ManualGainShape,

    /// AGC modes available on this backend. Empty `[]` = no AGC,
    /// only manual gain. Each entry's `id` is the wire string the
    /// backend understands when round-tripped via
    /// [`crate::GainSetting::AgcMode`].
    pub agc_modes: Vec<AgcMode>,

    /// Antenna-port choices. Empty `[]` = single fixed antenna,
    /// no selector rendered. Per-port frequency limits are
    /// enforced at `open()` time, not here.
    pub antennas: Vec<AntennaChoice>,

    /// Boolean toggles surfaced as plain checkboxes.
    pub features: BackendFeatures,

    /// Sample-rate selection — for now informational, but the GUI
    /// could surface it in a "Diagnostics" panel.
    pub sample_rate_strategy: SampleRateStrategy,
}

#[derive(Clone, Debug, Serialize)]
pub enum ManualGainShape {
    /// Continuous dB scale (Pluto AD9363 style, e.g. `[-3, 71]` step 1).
    DbContinuous {
        min_db: i32,
        max_db: i32,
        step_db: i32,
    },
    /// LNA state index + IF gain reduction in dB. SDRplay shape:
    /// `lna_states = 10`, `if_grdb_range = (20, 59)`, step 1.
    LnaPlusIf {
        lna_states: u8,
        if_grdb_range: (i32, i32),
        if_grdb_step: i32,
    },
    /// Discrete dB ladder (RTL-SDR rtl_test gain table). Listed so
    /// the next backend lands without enum-extension churn.
    DbDiscrete { steps_db: Vec<i32> },
}

#[derive(Clone, Debug, Serialize)]
pub struct AgcMode {
    /// Wire string accepted by [`crate::GainSetting::AgcMode`].
    /// E.g. `"manual"`, `"slow_attack"`, `"disable"`, `"fast"`.
    pub id: String,
    /// French UI label — the audience is FR-speaking radio amateurs
    /// (per CLAUDE.md). Examples: `"Manuel (gain fixe)"`,
    /// `"AGC rapide"`, `"AGC lente (défaut SDRplay)"`.
    pub label: String,
    /// True ⇒ all manual-gain inputs stay enabled (i.e. the mode
    /// is really "AGC off"). The "manual" mode of every backend
    /// sets this to `true`; AGC modes set it to `false`.
    pub manual: bool,
    /// Only meaningful when `manual = false` and the backend uses
    /// [`ManualGainShape::LnaPlusIf`]. True ⇒ the AGC loop only
    /// touches the IF gain reduction (`gRdB`); the LNA-state input
    /// stays operator-controlled. SDRplay AGC modes (slow/mid/fast)
    /// set this to `true`. The frontend keys the LNA `<input>`'s
    /// `disabled` attribute off this flag.
    #[serde(default)]
    pub keeps_lna_manual: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AntennaChoice {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BackendFeatures {
    pub bias_t: bool,
    pub fm_notch: bool,
    pub dab_notch: bool,
    pub ctcss_tx: bool,
    /// RF-bandwidth control range, when the backend exposes it
    /// (Pluto AD9361: 200 kHz – 56 MHz). `None` for SDRplay
    /// RSPduo (locked at 1.536 MHz).
    pub rf_bandwidth_range_hz: Option<(u64, u64)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SampleRateStrategy {
    /// Host-side IQ rate, after any in-API decimation. 576 kS/s
    /// for both Pluto and SDRplay today (= 48 × 12 — a small-prime
    /// composite that gives a clean polyphase decomposition to 48 kHz).
    pub host_iq_rate_hz: u64,
    /// Decimation ratio from `host_iq_rate_hz` to the audio rate
    /// (48 kHz). Always `host_iq_rate_hz / 48_000` for now.
    pub audio_decim_ratio: u32,
}
