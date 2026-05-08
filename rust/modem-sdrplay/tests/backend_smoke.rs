//! Smoke tests for the `SdrBackend` impl on SDRplay. No hardware
//! required — these only validate the static capabilities and the
//! `SdrConfig` ↔ `SdrplayConfig` mapping.

use modem_sdr::{
    DeviceDescriptor, GainSetting, ManualGainShape, ManualGainValue, SdrBackend, SdrConfig,
};
use modem_sdrplay::backend::{build_sdrplay_config, SdrplayBackend, BACKEND_ID};
use modem_sdrplay::device::{AgcMode, AntennaPort, Tuner};
use serde_json::json;

#[test]
fn sdrplay_capabilities_are_sane() {
    let backend = SdrplayBackend;
    assert_eq!(backend.id(), BACKEND_ID);
    let caps = backend.capabilities();
    assert!(caps.rx_supported);
    assert!(!caps.tx_supported);
    assert!(caps.tx_freq_range_hz.is_none());
    assert!(!caps.independent_rx_tx_freq);
    // Gain shape is the LNA + IF reduction one.
    match caps.manual_gain {
        ManualGainShape::LnaPlusIf {
            lna_states,
            if_grdb_range,
            if_grdb_step,
        } => {
            assert_eq!(lna_states, 10);
            assert_eq!(if_grdb_range, (20, 59));
            assert_eq!(if_grdb_step, 1);
        }
        _ => panic!("expected LnaPlusIf gain shape"),
    }
    // 4 AGC modes, exactly one is `manual` (the disable one).
    assert_eq!(caps.agc_modes.len(), 4);
    let manual_count = caps.agc_modes.iter().filter(|m| m.manual).count();
    assert_eq!(manual_count, 1);
    let disable = caps.agc_modes.iter().find(|m| m.id == "disable").unwrap();
    assert!(disable.manual);
    // Two antenna choices exposed (Hi-Z and 50 Ω).
    assert_eq!(caps.antennas.len(), 2);
    assert!(caps.antennas.iter().any(|a| a.id == "hiz"));
    assert!(caps.antennas.iter().any(|a| a.id == "fifty"));
    // Bias-T / FM notch / DAB notch all exposed; CTCSS TX not.
    assert!(caps.features.bias_t);
    assert!(caps.features.fm_notch);
    assert!(caps.features.dab_notch);
    assert!(!caps.features.ctcss_tx);
    // Bandwidth range = None (locked at 1.536 MHz internally).
    assert!(caps.features.rf_bandwidth_range_hz.is_none());
}

#[test]
fn build_sdrplay_config_maps_lna_if_gain() {
    let descriptor = DeviceDescriptor::new("sdrplay", "22340A2A34", "RSPduo");
    let mut cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "22340A2A34".into(),
        rx_freq_hz: 145_500_000,
        gain: GainSetting::Manual(ManualGainValue::LnaPlusIf {
            lna_state: 4,
            if_grdb: 40,
        }),
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tuner".into(), json!("B"));
    let scfg = build_sdrplay_config(&descriptor, &cfg).unwrap();
    assert_eq!(scfg.serial, "22340A2A34");
    assert_eq!(scfg.tuner, Tuner::B);
    assert_eq!(scfg.antenna, AntennaPort::Fifty);
    assert_eq!(scfg.lna_state, 4);
    assert_eq!(scfg.if_gain_reduction_db, 40);
    assert_eq!(scfg.agc_mode, AgcMode::Disable);
    assert_eq!(scfg.rf_freq_hz, 145_500_000);
}

#[test]
fn build_sdrplay_config_maps_agc_modes() {
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    for (id, want) in [
        ("disable", AgcMode::Disable),
        ("slow", AgcMode::Slow),
        ("mid", AgcMode::Mid),
        ("fast", AgcMode::Fast),
    ] {
        let mut cfg = SdrConfig {
            backend_id: "sdrplay".into(),
            device_id: "x".into(),
            gain: GainSetting::AgcMode { id: id.into() },
            antenna: "fifty".into(),
            ..SdrConfig::default()
        };
        cfg.backend_extras.insert("tuner".into(), json!("B"));
        let scfg = build_sdrplay_config(&descriptor, &cfg).unwrap();
        assert_eq!(scfg.agc_mode, want, "AGC mode mismatch for id={id}");
    }
}

#[test]
fn build_sdrplay_config_rejects_db_shape() {
    // Pluto-style continuous dB on SDRplay must be rejected.
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    let mut cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::Manual(ManualGainValue::Db { db: 30 }),
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tuner".into(), json!("B"));
    assert!(build_sdrplay_config(&descriptor, &cfg).is_err());
}

#[test]
fn build_sdrplay_config_requires_tuner_extra() {
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    // No `tuner` in backend_extras.
    let cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::AgcMode {
            id: "disable".into(),
        },
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    assert!(build_sdrplay_config(&descriptor, &cfg).is_err());
}

#[test]
fn build_sdrplay_config_rejects_unknown_tuner() {
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    let mut cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::AgcMode {
            id: "disable".into(),
        },
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tuner".into(), json!("Z"));
    assert!(build_sdrplay_config(&descriptor, &cfg).is_err());
}

#[test]
fn build_sdrplay_config_reads_decimation_extra() {
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    let mut cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::AgcMode {
            id: "mid".into(),
        },
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tuner".into(), json!("B"));
    cfg.backend_extras.insert("decimation".into(), json!(8));
    let scfg = build_sdrplay_config(&descriptor, &cfg).unwrap();
    assert_eq!(scfg.decimation, 8);
}
