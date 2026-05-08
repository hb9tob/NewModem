//! `modem_sdr::SdrBackend` implementation for SDRplay RX devices
//! (RSPduo + RSP1A wired today; RSP1 / RSP2 / RSPdx scaffolded but
//! gated off until their per-device sub-struct programming lands).
//!
//! [`SdrplayBackend`] is a zero-sized type that registers in
//! `modem-gui/src-tauri/src/sdr_registry.rs` behind the `sdrplay`
//! cargo feature. The whole family is **RX-only** — `tx_sink()`
//! returns `None` and the static [`BackendCapabilities`] reflects this.
//!
//! [`SdrplayDevice`] is the [`SdrDevice`] wrapper around an opened
//! daemon-side device handle. Construction calls
//! [`crate::device::open`] eagerly (unlike Pluto where opening is
//! deferred until `start_rx`/`tx_sink` runs) — the SDRplay daemon
//! is reference-counted internally, so an Open + Select cycle is
//! cheap and we'd rather know immediately whether the device is
//! reachable.
//!
//! ## SDRplay-specific keys recognized in [`SdrConfig::backend_extras`]
//!
//! - `tuner: "A" | "B"` — RSPduo only (the only multi-tuner part in
//!   the family). Optional; defaults to `"A"`. On RSP1A and other
//!   single-tuner parts the value is ignored — `device::open`
//!   overrides it to A before programming.
//! - `decimation: u32` — API-internal decimation factor (1, 2, 4, 8,
//!   16, 32). Default `4` (yields a 576 kS/s host I/Q rate from the
//!   2.304 MS/s master clock — same rate as Pluto, same downstream
//!   DSP chain).
//!
//! Unknown keys are silently ignored.

use std::sync::Arc;
use std::sync::OnceLock;

use modem_sdr::{
    AgcMode as SdrAgcModeDescriptor, AntennaChoice, BackendCapabilities, BackendFeatures,
    DeviceDescriptor, GainSetting, ManualGainShape, ManualGainValue, SampleRateStrategy,
    SdrBackend, SdrCaptureHandle, SdrConfig, SdrDevice, SdrError, TunerOption,
};

use crate::device::{
    self, AgcMode, AntennaPort, SdrplayConfig, SdrplayHardware, SdrplaySession, Tuner,
    PREFERRED_DECIMATION, PREFERRED_SAMPLE_RATE_HZ,
};
use crate::rx;

/// Stable backend ID. Matches the `sdrplay:` prefix the legacy GUI
/// code uses, so a saved `device_name = "sdrplay:22340A2A34"` parses
/// back to (this backend, "22340A2A34") through the registry.
pub const BACKEND_ID: &str = "sdrplay";
const DISPLAY_NAME: &str = "SDRplay";

// Per-hardware capability cells — populated lazily on first request.
// `SDRPLAY_CAPS_FAMILY` is the conservative pre-selection set the
// frontend uses before a device is picked (matches RSPduo because
// that's the most-featured part — never under-promises).
//
// Once the user picks a device, `capabilities_for(&descriptor)` reads
// the descriptor's `hardware_hint` and returns one of the per-device
// cells. RSP1B reuses the RSP1A cell (identical surface).
static SDRPLAY_CAPS_FAMILY: OnceLock<BackendCapabilities> = OnceLock::new();
static SDRPLAY_CAPS_RSPDUO: OnceLock<BackendCapabilities> = OnceLock::new();
static SDRPLAY_CAPS_RSP1A: OnceLock<BackendCapabilities> = OnceLock::new();
static SDRPLAY_CAPS_RSP1: OnceLock<BackendCapabilities> = OnceLock::new();

/// Wire string the SDRplay backend stamps in `descriptor.hardware_hint`
/// for each variant. Kept as constants so the matching in
/// [`capabilities_for_hint`] and the stamping in `list_devices` can't
/// drift apart.
pub const HINT_RSPDUO: &str = "rspduo";
pub const HINT_RSP1A: &str = "rsp1a";
pub const HINT_RSP1B: &str = "rsp1b";
pub const HINT_RSP1: &str = "rsp1";

/// Produce `descriptor.hardware_hint` from the daemon's `hwVer` byte.
/// Single source of truth — every other lookup goes through here.
fn hint_for_hardware(hw: SdrplayHardware) -> Option<&'static str> {
    match hw {
        SdrplayHardware::RspDuo => Some(HINT_RSPDUO),
        SdrplayHardware::Rsp1a => Some(HINT_RSP1A),
        SdrplayHardware::Rsp1b => Some(HINT_RSP1B),
        SdrplayHardware::Rsp1 => Some(HINT_RSP1),
        // RSP2 / RSPdx / RSPdxR2 / Unsupported(_) — list_devices still
        // surfaces them so the user sees the device, but `open()`
        // rejects with a clear error. No per-device caps cell yet.
        _ => None,
    }
}

/// AGC modes are identical across the SDRplay family — every part
/// runs the same AD8334-class IF VGA loop. Factored out so the four
/// per-device caps share one definition.
fn shared_agc_modes() -> Vec<SdrAgcModeDescriptor> {
    // SDRplay AGC only manages `gRdB` (IF gain reduction); the LNA
    // state stays operator-controlled in every AGC mode (cf.
    // `device::program_params` — `ctrl.agc.enable` does not touch
    // `tuner_params.gain.LNAstate`). The `keeps_lna_manual` flag
    // tells the GUI to leave the LNA-state `<input>` enabled while
    // disabling IF gRdB.
    vec![
        SdrAgcModeDescriptor {
            id: "disable".into(),
            label: "Manuel (gain fixe)".into(),
            manual: true,
            keeps_lna_manual: false,
        },
        SdrAgcModeDescriptor {
            id: "slow".into(),
            label: "AGC lente (5 Hz)".into(),
            manual: false,
            keeps_lna_manual: true,
        },
        SdrAgcModeDescriptor {
            id: "mid".into(),
            label: "AGC moyenne (50 Hz, défaut SDRplay)".into(),
            manual: false,
            keeps_lna_manual: true,
        },
        SdrAgcModeDescriptor {
            id: "fast".into(),
            label: "AGC rapide (100 Hz)".into(),
            manual: false,
            keeps_lna_manual: true,
        },
    ]
}

fn shared_sample_rate_strategy() -> SampleRateStrategy {
    SampleRateStrategy {
        host_iq_rate_hz: PREFERRED_SAMPLE_RATE_HZ / PREFERRED_DECIMATION as u64,
        audio_decim_ratio: 12,
    }
}

/// Family-level capabilities — used pre-selection, also serves as the
/// fallback for unknown hwVer bytes. Identical to RSPduo's caps so we
/// never silently hide a knob the user might need.
fn sdrplay_capabilities() -> &'static BackendCapabilities {
    SDRPLAY_CAPS_FAMILY.get_or_init(|| BackendCapabilities {
        rx_supported: true,
        tx_supported: false,
        // 1 kHz lower bound on RSPduo Tuner-A AM port, 2 GHz upper
        // bound. Other parts have similar ranges; the backend rejects
        // out-of-band tunings at open() anyway.
        rx_freq_range_hz: Some((1_000, 2_000_000_000)),
        tx_freq_range_hz: None,
        independent_rx_tx_freq: false,
        manual_gain: ManualGainShape::LnaPlusIf {
            lna_states: 10,
            if_grdb_range: (20, 59),
            if_grdb_step: 1,
        },
        agc_modes: shared_agc_modes(),
        antennas: vec![
            AntennaChoice {
                id: "hiz".into(),
                label: "Hi-Z (1 kHz – 60 MHz)".into(),
            },
            AntennaChoice {
                id: "fifty".into(),
                label: "50 Ω (60 MHz – 2 GHz)".into(),
            },
        ],
        tuner_options: vec![
            TunerOption {
                id: "A".into(),
                label: "A (Hi-Z + 50 Ω)".into(),
            },
            TunerOption {
                id: "B".into(),
                label: "B (50 Ω + bias-T)".into(),
            },
        ],
        features: BackendFeatures {
            bias_t: true,
            fm_notch: true,
            dab_notch: true,
            ctcss_tx: false,
            rf_bandwidth_range_hz: None,
        },
        sample_rate_strategy: shared_sample_rate_strategy(),
    })
}

/// RSPduo — same as the family caps. Kept as a separate cell so
/// future RSPduo-only tweaks (extRefOutput, dual-tuner mode) don't
/// leak into the family pre-selection set.
fn sdrplay_capabilities_rspduo() -> &'static BackendCapabilities {
    SDRPLAY_CAPS_RSPDUO.get_or_init(|| sdrplay_capabilities().clone())
}

/// RSP1A and RSP1B share this cell — both use `rsp1aParams` /
/// `rsp1aTunerParams` and ship identical knobs (bias-T on the single
/// SMA, FM/DAB notch). Differences (LO performance, ADC noise) live
/// at the silicon level, not in the API surface.
fn sdrplay_capabilities_rsp1a() -> &'static BackendCapabilities {
    SDRPLAY_CAPS_RSP1A.get_or_init(|| BackendCapabilities {
        rx_supported: true,
        tx_supported: false,
        rx_freq_range_hz: Some((1_000, 2_000_000_000)),
        tx_freq_range_hz: None,
        independent_rx_tx_freq: false,
        manual_gain: ManualGainShape::LnaPlusIf {
            lna_states: 10,
            if_grdb_range: (20, 59),
            if_grdb_step: 1,
        },
        agc_modes: shared_agc_modes(),
        // Single SMA — no antenna / tuner selectors. `[]` makes the
        // GUI hide both rows entirely.
        antennas: vec![],
        tuner_options: vec![],
        features: BackendFeatures {
            bias_t: true,
            fm_notch: true,
            dab_notch: true,
            ctcss_tx: false,
            rf_bandwidth_range_hz: None,
        },
        sample_rate_strategy: shared_sample_rate_strategy(),
    })
}

/// Original RSP1 — the slim one. No bias-T, no notch filters, single
/// fixed SMA, and a much smaller VHF gain table (4 LNA states 0-3 vs
/// 10 on RSP1A/B). Surfaces a stripped-down panel so toggles that
/// won't actually do anything stay hidden.
fn sdrplay_capabilities_rsp1() -> &'static BackendCapabilities {
    SDRPLAY_CAPS_RSP1.get_or_init(|| BackendCapabilities {
        rx_supported: true,
        tx_supported: false,
        rx_freq_range_hz: Some((1_000, 2_000_000_000)),
        tx_freq_range_hz: None,
        independent_rx_tx_freq: false,
        manual_gain: ManualGainShape::LnaPlusIf {
            // RSP1 VHF table — 4 states (0 = max gain, 3 = max
            // attenuation). The backend silently clamps higher
            // indices on open if the persisted config still has them.
            lna_states: 4,
            if_grdb_range: (20, 59),
            if_grdb_step: 1,
        },
        agc_modes: shared_agc_modes(),
        antennas: vec![],
        tuner_options: vec![],
        features: BackendFeatures {
            // RSP1 has none of these on the chip.
            bias_t: false,
            fm_notch: false,
            dab_notch: false,
            ctcss_tx: false,
            rf_bandwidth_range_hz: None,
        },
        sample_rate_strategy: shared_sample_rate_strategy(),
    })
}

/// Map a `hardware_hint` string back to the right per-device caps
/// cell. Used by the trait impl below — kept as a free function so
/// tests can exercise the dispatch without going through `SdrBackend`.
pub fn capabilities_for_hint(hint: Option<&str>) -> &'static BackendCapabilities {
    match hint {
        Some(HINT_RSPDUO) => sdrplay_capabilities_rspduo(),
        Some(HINT_RSP1A) | Some(HINT_RSP1B) => sdrplay_capabilities_rsp1a(),
        Some(HINT_RSP1) => sdrplay_capabilities_rsp1(),
        // No hint, or a hint we don't recognise (RSP2 / RSPdx) —
        // fall back to family caps. The `open()` path will reject
        // unsupported hwVer bytes with a clear error.
        _ => sdrplay_capabilities(),
    }
}

/// Zero-sized backend handle. Registered statically by the GUI.
#[derive(Clone, Copy, Debug, Default)]
pub struct SdrplayBackend;

impl SdrBackend for SdrplayBackend {
    fn id(&self) -> &'static str {
        BACKEND_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn capabilities(&self) -> &BackendCapabilities {
        sdrplay_capabilities()
    }

    fn capabilities_for(&self, descriptor: &DeviceDescriptor) -> &BackendCapabilities {
        // Read the hardware hint we stamped at `list_devices()` time
        // and route to the matching per-device caps cell. Anything
        // unstamped (or stamped with a hint we don't have a cell for)
        // falls back to the family caps — same as the trait default
        // would do.
        capabilities_for_hint(descriptor.hardware_hint.as_deref())
    }

    fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, SdrError> {
        // Daemon failures (not running, not installed, no device on
        // bus) are surfaced as an empty list — same UX as the libiio
        // path on Pluto. We want the friendly name to mention the
        // actual hardware (RSPduo / RSP1A / …) so the user can tell
        // them apart in the dropdown without looking at the serial,
        // and the `hardware_hint` so `capabilities_for` can refresh
        // the panel layout when the user picks a device.
        let metas = match device::list_devices_meta() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[sdrplay] list_devices_meta unavailable: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(metas
            .into_iter()
            .map(|(serial, hw)| {
                let friendly = format!("SDRplay {} — {serial}", hw.short_name());
                let mut desc = DeviceDescriptor::new(BACKEND_ID, serial, friendly);
                if let Some(hint) = hint_for_hardware(hw) {
                    desc = desc.with_hardware_hint(hint);
                }
                desc
            })
            .collect())
    }

    fn open(
        &self,
        descriptor: &DeviceDescriptor,
        config: &SdrConfig,
    ) -> Result<Box<dyn SdrDevice>, SdrError> {
        let sdrplay_config = build_sdrplay_config(descriptor, config)?;
        // Eager open — see module docs. The SdrplaySession owns the
        // daemon-side selection; dropping releases it.
        let session = device::open(&sdrplay_config).map_err(SdrError::backend)?;
        Ok(Box::new(SdrplayDevice {
            descriptor: descriptor.clone(),
            config: config.clone(),
            session: Some(session),
        }))
    }
}

/// Open RSPduo — owns the daemon-side device handle until dropped.
pub struct SdrplayDevice {
    descriptor: DeviceDescriptor,
    config: SdrConfig,
    /// Wrapped in `Option` so `start_rx` can `take()` it (the rx
    /// path consumes the session by value to spawn the supervisor
    /// thread).
    session: Option<SdrplaySession>,
}

impl SdrDevice for SdrplayDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.descriptor
    }

    fn config(&self) -> &SdrConfig {
        &self.config
    }

    fn capabilities(&self) -> &BackendCapabilities {
        // Mirror what `SdrBackend::capabilities_for` returns for the
        // descriptor we were opened with — once the user picks a
        // device, every consumer should see the per-device caps.
        capabilities_for_hint(self.descriptor.hardware_hint.as_deref())
    }

    fn start_rx(&mut self) -> Result<(SdrCaptureHandle, std::sync::mpsc::Receiver<Vec<f32>>), SdrError> {
        let session = self.session.take().ok_or_else(|| SdrError::NotSupported {
            backend: BACKEND_ID,
            detail: "device already started — re-open to RX again".into(),
        })?;
        let (handle, rx) = rx::start_on(session).map_err(SdrError::backend)?;
        Ok((SdrCaptureHandle::new(handle), rx))
    }

    fn tx_sink(&self) -> Option<Arc<dyn modem_io::SampleSink>> {
        // RSPduo is RX-only hardware. No TX adapter to give back.
        None
    }

    fn update_rx_freq(&mut self, _hz: u64) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
    fn update_tx_freq(&mut self, _hz: u64) -> Result<(), SdrError> {
        Err(SdrError::NotSupported {
            backend: BACKEND_ID,
            detail: "RSPduo is RX-only".into(),
        })
    }
    fn update_gain(&mut self, _gain: GainSetting) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
    fn update_antenna(&mut self, _choice: AntennaChoice) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
}

/// Translate a generic [`SdrConfig`] into the SDRplay-specific
/// [`SdrplayConfig`]. Validates that the gain shape, AGC mode ID,
/// antenna ID, and tuner key match what this backend advertises.
pub fn build_sdrplay_config(
    descriptor: &DeviceDescriptor,
    cfg: &SdrConfig,
) -> Result<SdrplayConfig, SdrError> {
    // Gain — RSPduo has only the LnaPlusIf shape.
    let (lna_state, if_gain_reduction_db, agc_mode) = match &cfg.gain {
        GainSetting::Manual(ManualGainValue::LnaPlusIf { lna_state, if_grdb }) => {
            (*lna_state, *if_grdb, AgcMode::Disable)
        }
        GainSetting::Manual(other) => {
            return Err(SdrError::InvalidConfig {
                field: "gain",
                detail: format!("SDRplay expects Manual::LnaPlusIf, got {other:?}"),
            });
        }
        GainSetting::AgcMode { id, lna_state } => {
            let mode = parse_agc_mode(id)?;
            // gRdB is daemon-managed under AGC, but SdrplayConfig
            // still wants an i32 — pass a sane default. The LNA
            // state stays operator-controlled (the daemon's AGC
            // loop does not touch `tuner_params.gain.LNAstate`):
            // honour the GUI's hint when it sent one, else fall
            // back to the typical mid-band index 4.
            let lna = lna_state.unwrap_or(4);
            (lna, 40, mode)
        }
    };

    // Antenna — empty string is OK on Tuner B (no antenna selector).
    // We carry it through unconditionally; the daemon ignores antenna
    // settings on Tuner B anyway.
    let antenna = parse_antenna(&cfg.antenna)?;

    // Tuner extras — optional. The RSPduo is the only multi-tuner
    // part, and `device::open` overrides this to Tuner::A on every
    // single-tuner device anyway. We default to A here so an RSP1A
    // config (which has no `tuner` extras key) builds cleanly; an
    // explicit invalid string still errors out so a typo on RSPduo
    // surfaces immediately.
    let tuner = match cfg
        .backend_extras
        .get("tuner")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_uppercase())
    {
        Some(s) if s == "A" => Tuner::A,
        Some(s) if s == "B" => Tuner::B,
        Some(other) => {
            return Err(SdrError::InvalidConfig {
                field: "backend_extras.tuner",
                detail: format!("expected 'A' or 'B', got '{other}'"),
            });
        }
        None => Tuner::A,
    };

    // Decimation — defaults to PREFERRED_DECIMATION (4) so the host
    // I/Q rate matches the Pluto code path's downstream DSP.
    let decimation = cfg
        .backend_extras
        .get("decimation")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(PREFERRED_DECIMATION);

    Ok(SdrplayConfig {
        serial: descriptor.id.clone(),
        tuner,
        antenna,
        bias_t: cfg.bias_t,
        fm_notch: cfg.fm_notch,
        dab_notch: cfg.dab_notch,
        rf_freq_hz: cfg.rx_freq_hz,
        sample_rate_hz: PREFERRED_SAMPLE_RATE_HZ as f64,
        decimation,
        lna_state,
        if_gain_reduction_db,
        agc_mode,
        max_deviation_hz: cfg.max_deviation_hz,
    })
}

fn parse_agc_mode(id: &str) -> Result<AgcMode, SdrError> {
    match id {
        "disable" => Ok(AgcMode::Disable),
        "slow" => Ok(AgcMode::Slow),
        "mid" => Ok(AgcMode::Mid),
        "fast" => Ok(AgcMode::Fast),
        other => Err(SdrError::InvalidConfig {
            field: "gain",
            detail: format!("unknown AGC mode '{other}' for SDRplay"),
        }),
    }
}

fn parse_antenna(id: &str) -> Result<AntennaPort, SdrError> {
    match id {
        "" | "fifty" => Ok(AntennaPort::Fifty),
        "hiz" => Ok(AntennaPort::Hiz),
        other => Err(SdrError::InvalidConfig {
            field: "antenna",
            detail: format!("unknown antenna '{other}' for SDRplay"),
        }),
    }
}
