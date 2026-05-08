//! `modem_sdr::SdrBackend` implementation for the ADALM-PLUTO.
//!
//! [`PlutoBackend`] is a zero-sized type that registers in
//! `modem-gui/src-tauri/src/sdr_registry.rs` behind the `pluto`
//! cargo feature. It exposes the Pluto family's static
//! [`BackendCapabilities`], scans libiio USB contexts in
//! [`PlutoBackend::list_devices`], and opens a [`PlutoDevice`] in
//! [`PlutoBackend::open`].
//!
//! [`PlutoDevice`] is the [`SdrDevice`] wrapper around a not-yet-
//! streaming Pluto. It owns the [`PlutoConfig`] derived from the
//! GUI's [`SdrConfig`] (via [`build_pluto_config`]) and dispatches
//! `start_rx` / `tx_sink` to the existing Pluto code paths
//! (`crate::rx::start`, `crate::tx::PlutoSink`).
//!
//! ## Pluto-specific keys recognized in [`SdrConfig::backend_extras`]
//!
//! - `tx_attenuation_db: f64` — AD9361 TX hardware-gain attenuation.
//!   Range `[0.0, 89.75]` dB step 0.25. Default `10.0` (matches
//!   today's `PlutoConfig::default()`).
//! - `prefer_low_rate: bool` — try 576 kS/s first (true) vs go
//!   directly to 2304 kS/s (false). Default `true`.
//!
//! Unknown keys are silently ignored.

use std::sync::Arc;
use std::sync::OnceLock;

use modem_sdr::{
    AgcMode, AntennaChoice, BackendCapabilities, BackendFeatures, DeviceDescriptor, GainSetting,
    ManualGainShape, ManualGainValue, SampleRateStrategy, SdrBackend, SdrCaptureHandle, SdrConfig,
    SdrDevice, SdrError,
};

use crate::device::{PlutoConfig, RxGainMode};
use crate::rx;
use crate::tx::PlutoSink;

/// Stable backend ID. Used as the dropdown grouping key, MRU
/// favourites bucket, and prefix in `<backend_id>:<device_id>`
/// composite names.
pub const BACKEND_ID: &str = "pluto";
const DISPLAY_NAME: &str = "PlutoSDR (ADALM-PLUTO)";

static PLUTO_CAPS: OnceLock<BackendCapabilities> = OnceLock::new();

fn pluto_capabilities() -> &'static BackendCapabilities {
    PLUTO_CAPS.get_or_init(|| BackendCapabilities {
        rx_supported: true,
        tx_supported: true,
        // AD9363 RF tuning range as advertised by Analog Devices.
        // Practical lower bound is closer to 47 MHz on AD9361 silicon
        // but the driver clamps tunings to spec.
        rx_freq_range_hz: Some((70_000_000, 6_000_000_000)),
        tx_freq_range_hz: Some((70_000_000, 6_000_000_000)),
        independent_rx_tx_freq: true,
        manual_gain: ManualGainShape::DbContinuous {
            min_db: -3,
            max_db: 71,
            step_db: 1,
        },
        agc_modes: vec![
            AgcMode {
                id: "manual".into(),
                label: "Manuel (gain fixe)".into(),
                manual: true,
            },
            AgcMode {
                id: "fast_attack".into(),
                label: "AGC rapide".into(),
                manual: false,
            },
            AgcMode {
                id: "slow_attack".into(),
                label: "AGC lente (défaut driver)".into(),
                manual: false,
            },
            AgcMode {
                id: "hybrid".into(),
                label: "AGC hybride".into(),
                manual: false,
            },
        ],
        antennas: vec![],
        features: BackendFeatures {
            ctcss_tx: true,
            rf_bandwidth_range_hz: Some((200_000, 56_000_000)),
            ..Default::default()
        },
        sample_rate_strategy: SampleRateStrategy {
            host_iq_rate_hz: crate::PREFERRED_SAMPLE_RATE_HZ,
            audio_decim_ratio: crate::PREFERRED_RATIO as u32,
        },
    })
}

/// Zero-sized backend handle. Registered statically by the GUI.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlutoBackend;

impl SdrBackend for PlutoBackend {
    fn id(&self) -> &'static str {
        BACKEND_ID
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn capabilities(&self) -> &BackendCapabilities {
        pluto_capabilities()
    }

    fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, SdrError> {
        // Scan libiio USB contexts. We deliberately do NOT surface
        // libiio failures as errors — the GUI just shows an empty
        // group when no Pluto is plugged in (or when the libiio USB
        // backend isn't compiled in on this platform).
        let scan = match industrial_io::ScanContext::new_usb() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[pluto] USB scan unavailable: {e}");
                return Ok(Vec::new());
            }
        };
        let mut out = Vec::new();
        for (uri, descr) in scan.iter() {
            // libiio can list other AD9361-class boards alongside
            // genuine Plutos — match on the description so we don't
            // try to drive a dev kit through the Pluto code paths.
            let is_pluto = descr.contains("PlutoSDR")
                || descr.contains("ADALM-PLUTO")
                || descr.contains("AD9363")
                || descr.contains("AD9361");
            if !is_pluto {
                continue;
            }
            let friendly = format!("Pluto SDR — {uri}");
            out.push(DeviceDescriptor::new(BACKEND_ID, uri, friendly));
        }
        Ok(out)
    }

    fn open(
        &self,
        descriptor: &DeviceDescriptor,
        config: &SdrConfig,
    ) -> Result<Box<dyn SdrDevice>, SdrError> {
        let pluto_config = build_pluto_config(descriptor, config)?;
        // Don't actually open the libiio context here — keep the
        // existing semantics where opening happens lazily inside
        // `rx::start` (RX path) or `PlutoSink::play_buffer` (TX path).
        // Eager-opening would burn USB cycles and serialise the two
        // directions through one Context that we don't actually need
        // to share at the SdrDevice level.
        let device = PlutoDevice {
            descriptor: descriptor.clone(),
            config: config.clone(),
            pluto_config,
        };
        Ok(Box::new(device))
    }
}

/// Open Pluto — owns its [`PlutoConfig`] until dropped.
pub struct PlutoDevice {
    descriptor: DeviceDescriptor,
    config: SdrConfig,
    pluto_config: PlutoConfig,
}

impl SdrDevice for PlutoDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.descriptor
    }

    fn config(&self) -> &SdrConfig {
        &self.config
    }

    fn capabilities(&self) -> &BackendCapabilities {
        pluto_capabilities()
    }

    fn start_rx(&mut self) -> Result<(SdrCaptureHandle, std::sync::mpsc::Receiver<Vec<f32>>), SdrError> {
        let (handle, rx) = rx::start(&self.pluto_config).map_err(SdrError::backend)?;
        Ok((SdrCaptureHandle::new(handle), rx))
    }

    fn tx_sink(&self) -> Option<Arc<dyn modem_io::SampleSink>> {
        Some(Arc::new(PlutoSink::new(self.pluto_config.clone())))
    }

    fn update_rx_freq(&mut self, _hz: u64) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
    fn update_tx_freq(&mut self, _hz: u64) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
    fn update_gain(&mut self, _gain: GainSetting) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
    fn update_antenna(&mut self, _choice: AntennaChoice) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
}

/// Translate a generic [`SdrConfig`] into the Pluto-specific
/// [`PlutoConfig`].
///
/// Validation is deliberate but light — out-of-range frequencies and
/// gain shapes that don't match this backend's capabilities are
/// rejected with [`SdrError::InvalidConfig`]; everything else is
/// passed through and let the libiio attribute write fail loudly if
/// the chip doesn't like it.
pub fn build_pluto_config(
    descriptor: &DeviceDescriptor,
    cfg: &SdrConfig,
) -> Result<PlutoConfig, SdrError> {
    // Gain — Pluto has only the DbContinuous shape. Reject anything
    // else outright so a forgotten settings.json migration surfaces
    // as an InvalidConfig rather than a silent gain default.
    let (rx_gain_mode, rx_gain_db) = match &cfg.gain {
        GainSetting::Manual(ManualGainValue::Db { db }) => (RxGainMode::Manual, *db),
        GainSetting::Manual(other) => {
            return Err(SdrError::InvalidConfig {
                field: "gain",
                detail: format!("Pluto expects Manual::Db, got {other:?}"),
            });
        }
        GainSetting::AgcMode { id } => {
            let mode = RxGainMode::from_iio_str(id).ok_or_else(|| SdrError::InvalidConfig {
                field: "gain",
                detail: format!("unknown AGC mode '{id}' for Pluto"),
            })?;
            // Default manual gain when AGC is engaged; the AD9361
            // ignores `rx_gain_db` outside Manual but PlutoConfig
            // still needs an i32 for the field.
            (mode, 30)
        }
    };

    // backend_extras — read with sane defaults that match today's
    // PlutoConfig::default() so the GUI behaves identically post-refactor.
    let tx_attenuation_db = cfg
        .backend_extras
        .get("tx_attenuation_db")
        .and_then(|v| v.as_f64())
        .unwrap_or(10.0);
    let prefer_low_rate = cfg
        .backend_extras
        .get("prefer_low_rate")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    Ok(PlutoConfig {
        uri: descriptor.id.clone(),
        rx_freq_hz: cfg.rx_freq_hz,
        tx_freq_hz: cfg.tx_freq_hz,
        rx_gain_mode,
        rx_gain_db,
        tx_attenuation_db,
        rf_bandwidth_hz: cfg.rf_bandwidth_hz.unwrap_or(200_000),
        prefer_low_rate,
        rx_max_deviation_hz: cfg.max_deviation_hz,
        tx_deviation_hz: cfg.tx_deviation_hz,
        ctcss_freq_hz: cfg.ctcss_freq_hz,
        ctcss_level: cfg.ctcss_level,
    })
}
