//! `modem_sdr::SdrBackend` implementation for RTL-SDR (Blog V3, V4, and
//! any compatible RTL2832U + R820T-family dongle).
//!
//! [`RtlsdrBackend`] is a zero-sized type registered by
//! `modem-gui/src-tauri/src/sdr_registry.rs` behind the `rtlsdr`
//! cargo feature. The family is **RX-only** — `tx_sink()` returns
//! `None` and the static [`BackendCapabilities`] reflects this.
//!
//! Runtime loading: every call that touches librtlsdr (list_devices,
//! open) goes through [`crate::ffi::RtlsdrLib::get`], which `dlopen`s
//! the library once and caches the result. When the library can't be
//! resolved, `list_devices()` returns `Ok(Vec::new())` and the GUI's
//! Paramètres status (via [`RtlsdrBackend::library_available`])
//! reports `false` so the user gets a "Bibliothèque manquante"
//! diagnostic rather than a silent empty dropdown.

use std::sync::Arc;
use std::sync::OnceLock;

use modem_sdr::{
    AgcMode as SdrAgcModeDescriptor, AntennaChoice, BackendCapabilities, BackendFeatures,
    DeviceDescriptor, GainSetting, ManualGainShape, ManualGainValue, SampleRateStrategy,
    SdrBackend, SdrCaptureHandle, SdrConfig, SdrDevice, SdrError, TunerOption,
};

use crate::device::{self, RtlsdrConfig, GAIN_TABLE_TENTHS_DB, PREFERRED_SAMPLE_RATE_HZ};
use crate::ffi::RtlsdrLib;
use crate::rx;

pub const BACKEND_ID: &str = "rtlsdr";
const DISPLAY_NAME: &str = "RTL-SDR (RTL2832U)";

static RTLSDR_CAPS: OnceLock<BackendCapabilities> = OnceLock::new();

/// Family-level capabilities. Same for V3 and V4 — the only practical
/// difference (V3's direct-sampling HF mode) is gated behind a
/// `backend_extras["direct_sampling"]` opt-in, not a different caps
/// shape.
fn rtlsdr_capabilities() -> &'static BackendCapabilities {
    RTLSDR_CAPS.get_or_init(|| {
        // The dB ladder shown in the GUI. Tenths-of-dB internally but
        // ManualGainShape::DbDiscrete is a Vec<i32> of integer dB, so
        // we round each step. A few adjacent rounded values collide
        // (e.g. 0.9 and 1.4 both round to 1, 43.9 and 44.5 round to 44
        // and 45) — acceptable for display; the underlying step_idx
        // still resolves to the correct programmed value.
        let steps_db: Vec<i32> = GAIN_TABLE_TENTHS_DB
            .iter()
            .map(|&t| (t as f32 / 10.0).round() as i32)
            .collect();
        BackendCapabilities {
            rx_supported: true,
            tx_supported: false,
            // R820T/R828D direct tuner range. HF via V3 direct-sampling
            // or V4 upconverter is a follow-up; not advertised here.
            rx_freq_range_hz: Some((24_000_000, 1_766_000_000)),
            tx_freq_range_hz: None,
            independent_rx_tx_freq: false,
            manual_gain: ManualGainShape::DbDiscrete { steps_db },
            agc_modes: shared_agc_modes(),
            // Single fixed SMA — no antenna or tuner selector.
            antennas: vec![] as Vec<AntennaChoice>,
            tuner_options: vec![] as Vec<TunerOption>,
            features: BackendFeatures {
                bias_t: true,
                fm_notch: false,
                dab_notch: false,
                ctcss_tx: false,
                rf_bandwidth_range_hz: None,
            },
            sample_rate_strategy: SampleRateStrategy {
                host_iq_rate_hz: PREFERRED_SAMPLE_RATE_HZ as u64,
                audio_decim_ratio: PREFERRED_SAMPLE_RATE_HZ / 48_000,
            },
        }
    })
}

fn shared_agc_modes() -> Vec<SdrAgcModeDescriptor> {
    vec![
        SdrAgcModeDescriptor {
            id: "manual".into(),
            label: "Manuel (gain fixe)".into(),
            manual: true,
            keeps_lna_manual: false,
        },
        SdrAgcModeDescriptor {
            id: "tuner_agc".into(),
            label: "AGC tuner".into(),
            manual: false,
            keeps_lna_manual: false,
        },
    ]
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RtlsdrBackend;

impl RtlsdrBackend {
    /// Quick check used by the GUI to paint the Paramètres status. Does
    /// trigger the one-shot dlopen if it hasn't already happened —
    /// that's fine: the user just ticked the checkbox, so this is the
    /// natural moment to know whether the library is reachable.
    pub fn library_available(&self) -> bool {
        RtlsdrLib::get().is_ok()
    }
}

impl SdrBackend for RtlsdrBackend {
    fn id(&self) -> &'static str {
        BACKEND_ID
    }
    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }
    fn capabilities(&self) -> &BackendCapabilities {
        rtlsdr_capabilities()
    }
    fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, SdrError> {
        let metas = match device::list_devices_meta() {
            Ok(v) => v,
            // DllMissing is already turned into Ok(Vec::new()) inside
            // list_devices_meta; anything else is a genuine error.
            Err(e) => {
                eprintln!("[rtlsdr] list_devices_meta unavailable: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(metas
            .into_iter()
            .map(|(id, friendly, hw)| {
                DeviceDescriptor::new(BACKEND_ID, id, friendly)
                    .with_hardware_hint(hw.hint())
            })
            .collect())
    }
    fn open(
        &self,
        descriptor: &DeviceDescriptor,
        config: &SdrConfig,
    ) -> Result<Box<dyn SdrDevice>, SdrError> {
        let rtlsdr_config = build_rtlsdr_config(descriptor, config)?;
        let session = device::open(&rtlsdr_config).map_err(SdrError::backend)?;
        Ok(Box::new(RtlsdrDevice {
            descriptor: descriptor.clone(),
            config: config.clone(),
            session: Some(session),
        }))
    }
}

/// One opened RTL-SDR dongle. Owns the librtlsdr handle until dropped
/// — the inner `RtlsdrSession`'s Drop closes the USB claim.
pub struct RtlsdrDevice {
    descriptor: DeviceDescriptor,
    config: SdrConfig,
    /// Wrapped in `Option` so `start_rx` can `take()` it (the rx
    /// path consumes the session by value to spawn the capture
    /// thread).
    session: Option<device::RtlsdrSession>,
}

impl SdrDevice for RtlsdrDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.descriptor
    }
    fn config(&self) -> &SdrConfig {
        &self.config
    }
    fn capabilities(&self) -> &BackendCapabilities {
        rtlsdr_capabilities()
    }
    fn start_rx(
        &mut self,
    ) -> Result<(SdrCaptureHandle, std::sync::mpsc::Receiver<Vec<f32>>), SdrError> {
        let session = self.session.take().ok_or_else(|| SdrError::NotSupported {
            backend: BACKEND_ID,
            detail: "device already started — re-open to RX again".into(),
        })?;
        let (handle, rx) = rx::start_on(session).map_err(SdrError::backend)?;
        Ok((SdrCaptureHandle::new(handle), rx))
    }
    fn tx_sink(&self) -> Option<Arc<dyn modem_io::SampleSink>> {
        None
    }
    fn update_rx_freq(&mut self, _hz: u64) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
    fn update_tx_freq(&mut self, _hz: u64) -> Result<(), SdrError> {
        Err(SdrError::NotSupported {
            backend: BACKEND_ID,
            detail: "RTL-SDR is RX-only".into(),
        })
    }
    fn update_gain(&mut self, _gain: GainSetting) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
    fn update_antenna(&mut self, _choice: AntennaChoice) -> Result<(), SdrError> {
        Err(SdrError::NotImplemented)
    }
}

/// Translate a backend-agnostic [`SdrConfig`] into the RTL-SDR-specific
/// [`RtlsdrConfig`]. Validates the gain shape and AGC mode ID match
/// what this backend advertises.
pub fn build_rtlsdr_config(
    descriptor: &DeviceDescriptor,
    cfg: &SdrConfig,
) -> Result<RtlsdrConfig, SdrError> {
    let (gain_step_idx, tuner_agc_enabled) = match &cfg.gain {
        GainSetting::Manual(ManualGainValue::Discrete { step_idx }) => (
            (*step_idx).min(GAIN_TABLE_TENTHS_DB.len() - 1),
            false,
        ),
        GainSetting::Manual(other) => {
            return Err(SdrError::InvalidConfig {
                field: "gain",
                detail: format!("RTL-SDR expects Manual::Discrete, got {other:?}"),
            });
        }
        GainSetting::AgcMode { id, .. } => match id.as_str() {
            "manual" => (15, false),
            "tuner_agc" => (15, true),
            other => {
                return Err(SdrError::InvalidConfig {
                    field: "gain",
                    detail: format!("unknown AGC mode '{other}' for RTL-SDR"),
                });
            }
        },
    };

    let ppm_correction = cfg
        .backend_extras
        .get("ppm_correction")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .unwrap_or(0);

    let direct_sampling = cfg
        .backend_extras
        .get("direct_sampling")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let tuner_bandwidth_hz = cfg
        .backend_extras
        .get("tuner_bandwidth_hz")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(0);

    // descriptor.id round-trips back as RtlsdrConfig::serial — empty
    // string is honoured (= first dongle).
    Ok(RtlsdrConfig {
        serial: descriptor.id.clone(),
        rf_freq_hz: cfg.rx_freq_hz,
        sample_rate_hz: PREFERRED_SAMPLE_RATE_HZ,
        gain_step_idx,
        tuner_agc_enabled,
        rtl_agc_enabled: false,
        bias_t: cfg.bias_t,
        ppm_correction,
        direct_sampling,
        tuner_bandwidth_hz,
        max_deviation_hz: cfg.max_deviation_hz,
    })
}
