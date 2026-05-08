//! `modem_sdr::SdrBackend` implementation for the SDRplay RSPduo.
//!
//! [`SdrplayBackend`] is a zero-sized type that registers in
//! `modem-gui/src-tauri/src/sdr_registry.rs` behind the `sdrplay`
//! cargo feature. RSPduo is **RX-only** — `tx_sink()` returns `None`
//! and the static [`BackendCapabilities`] reflects this.
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
//! - `tuner: "A" | "B"` — required (no default). Selects the RSPduo
//!   tuner half. Default at the GUI layer is `"B"` for VHF (matches
//!   `SdrplayConfig::default()`).
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
    SdrBackend, SdrCaptureHandle, SdrConfig, SdrDevice, SdrError,
};

use crate::device::{
    self, AgcMode, AntennaPort, SdrplayConfig, SdrplaySession, Tuner, PREFERRED_DECIMATION,
    PREFERRED_SAMPLE_RATE_HZ,
};
use crate::rx;

/// Stable backend ID. Matches the `sdrplay:` prefix the legacy GUI
/// code uses, so a saved `device_name = "sdrplay:22340A2A34"` parses
/// back to (this backend, "22340A2A34") through the registry.
pub const BACKEND_ID: &str = "sdrplay";
const DISPLAY_NAME: &str = "SDRplay RSPduo";

static SDRPLAY_CAPS: OnceLock<BackendCapabilities> = OnceLock::new();

fn sdrplay_capabilities() -> &'static BackendCapabilities {
    SDRPLAY_CAPS.get_or_init(|| BackendCapabilities {
        rx_supported: true,
        tx_supported: false,
        // RSPduo tuning range: 1 kHz lower bound on Tuner-A AM port,
        // 2 GHz upper bound. The backend rejects out-of-port-range
        // tunings at open() — the GUI just needs to know the family
        // bound for the freq-input min/max attribute.
        rx_freq_range_hz: Some((1_000, 2_000_000_000)),
        tx_freq_range_hz: None,
        independent_rx_tx_freq: false,
        manual_gain: ManualGainShape::LnaPlusIf {
            // RSPduo VHF gain table has 10 LNA states (0 = least
            // attenuation = most front-end gain).
            lna_states: 10,
            if_grdb_range: (20, 59),
            if_grdb_step: 1,
        },
        agc_modes: vec![
            SdrAgcModeDescriptor {
                id: "disable".into(),
                label: "Manuel (gain fixe)".into(),
                manual: true,
            },
            SdrAgcModeDescriptor {
                id: "slow".into(),
                label: "AGC lente (5 Hz)".into(),
                manual: false,
            },
            SdrAgcModeDescriptor {
                id: "mid".into(),
                label: "AGC moyenne (50 Hz, défaut SDRplay)".into(),
                manual: false,
            },
            SdrAgcModeDescriptor {
                id: "fast".into(),
                label: "AGC rapide (100 Hz)".into(),
                manual: false,
            },
        ],
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
        features: BackendFeatures {
            bias_t: true,
            fm_notch: true,
            dab_notch: true,
            ctcss_tx: false,
            // Bandwidth is locked at 1.536 MHz on this RX-only setup
            // (set inside `device::program_params`); no runtime knob.
            rf_bandwidth_range_hz: None,
        },
        sample_rate_strategy: SampleRateStrategy {
            host_iq_rate_hz: PREFERRED_SAMPLE_RATE_HZ / PREFERRED_DECIMATION as u64,
            audio_decim_ratio: 12,
        },
    })
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

    fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, SdrError> {
        // Daemon failures (not running, not installed, no device on
        // bus) are surfaced as an empty list — same UX as the libiio
        // path on Pluto.
        let serials = match device::list_serials() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[sdrplay] list_serials unavailable: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(serials
            .into_iter()
            .map(|s| {
                let friendly = format!("SDRplay RSPduo — {s}");
                DeviceDescriptor::new(BACKEND_ID, s, friendly)
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
        sdrplay_capabilities()
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
        GainSetting::AgcMode { id } => {
            let mode = parse_agc_mode(id)?;
            // When AGC is on, gRdB is daemon-managed but
            // SdrplayConfig still wants a value — pass a sane default
            // and leave LNA at the typical mid-band index.
            (4, 40, mode)
        }
    };

    // Antenna — empty string is OK on Tuner B (no antenna selector).
    // We carry it through unconditionally; the daemon ignores antenna
    // settings on Tuner B anyway.
    let antenna = parse_antenna(&cfg.antenna)?;

    // Tuner is required (no sensible default — picking the wrong
    // tuner would silently send the user to the wrong RF path). Read
    // from backend_extras.
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
        None => {
            return Err(SdrError::InvalidConfig {
                field: "backend_extras.tuner",
                detail: "RSPduo requires tuner = 'A' or 'B'".into(),
            });
        }
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
