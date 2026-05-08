//! Smoke tests for the `SdrBackend` impl on Pluto. No hardware
//! required — these only validate the static capabilities and the
//! `SdrConfig` ↔ `PlutoConfig` mapping.

use modem_pluto::backend::{build_pluto_config, PlutoBackend, BACKEND_ID};
use modem_pluto::device::RxGainMode;
use modem_sdr::{
    DeviceDescriptor, GainSetting, ManualGainShape, ManualGainValue, SdrBackend, SdrConfig,
};
use serde_json::json;

#[test]
fn pluto_capabilities_are_sane() {
    let backend = PlutoBackend;
    assert_eq!(backend.id(), BACKEND_ID);
    let caps = backend.capabilities();
    assert!(caps.rx_supported && caps.tx_supported);
    assert!(caps.independent_rx_tx_freq);
    let (rx_lo, rx_hi) = caps.rx_freq_range_hz.unwrap();
    assert!(rx_lo < rx_hi);
    assert_eq!(rx_lo, 70_000_000);
    // Gain shape is the AD9363 continuous one.
    match caps.manual_gain {
        ManualGainShape::DbContinuous { min_db, max_db, step_db } => {
            assert_eq!(min_db, -3);
            assert_eq!(max_db, 71);
            assert_eq!(step_db, 1);
        }
        _ => panic!("expected DbContinuous"),
    }
    // The four AD9361 AGC modes are present, exactly one is `manual`.
    assert_eq!(caps.agc_modes.len(), 4);
    let manual_count = caps.agc_modes.iter().filter(|m| m.manual).count();
    assert_eq!(manual_count, 1);
    // CTCSS TX advertised, RF bandwidth range present.
    assert!(caps.features.ctcss_tx);
    assert!(caps.features.rf_bandwidth_range_hz.is_some());
    // No antenna selector on Pluto.
    assert!(caps.antennas.is_empty());
}

#[test]
fn build_pluto_config_maps_manual_gain() {
    let descriptor = DeviceDescriptor::new("pluto", "usb:1.6.5", "Pluto SDR — usb:1.6.5");
    let cfg = SdrConfig {
        backend_id: "pluto".into(),
        device_id: "usb:1.6.5".into(),
        rx_freq_hz: 145_500_000,
        tx_freq_hz: 145_500_000,
        gain: GainSetting::Manual(ManualGainValue::Db { db: 42 }),
        max_deviation_hz: 5000.0,
        tx_deviation_hz: 5000.0,
        rf_bandwidth_hz: Some(250_000),
        ..SdrConfig::default()
    };
    let pcfg = build_pluto_config(&descriptor, &cfg).unwrap();
    assert_eq!(pcfg.uri, "usb:1.6.5");
    assert_eq!(pcfg.rx_freq_hz, 145_500_000);
    assert_eq!(pcfg.rx_gain_mode, RxGainMode::Manual);
    assert_eq!(pcfg.rx_gain_db, 42);
    assert_eq!(pcfg.rf_bandwidth_hz, 250_000);
    // Defaults from backend_extras when missing.
    assert_eq!(pcfg.tx_attenuation_db, 10.0);
    assert!(pcfg.prefer_low_rate);
}

#[test]
fn build_pluto_config_maps_agc() {
    let descriptor = DeviceDescriptor::new("pluto", "usb:1.6.5", "x");
    let cfg = SdrConfig {
        backend_id: "pluto".into(),
        device_id: "usb:1.6.5".into(),
        gain: GainSetting::AgcMode {
            id: "fast_attack".into(),
            lna_state: None,
        },
        ..SdrConfig::default()
    };
    let pcfg = build_pluto_config(&descriptor, &cfg).unwrap();
    assert_eq!(pcfg.rx_gain_mode, RxGainMode::FastAttack);
}

#[test]
fn build_pluto_config_rejects_lna_shape() {
    // SDRplay-style gain on Pluto must be rejected — the GUI is
    // expected to feed only shapes that match capabilities.manual_gain.
    let descriptor = DeviceDescriptor::new("pluto", "usb:1.6.5", "x");
    let cfg = SdrConfig {
        backend_id: "pluto".into(),
        device_id: "usb:1.6.5".into(),
        gain: GainSetting::Manual(ManualGainValue::LnaPlusIf {
            lna_state: 4,
            if_grdb: 40,
        }),
        ..SdrConfig::default()
    };
    assert!(build_pluto_config(&descriptor, &cfg).is_err());
}

#[test]
fn build_pluto_config_reads_backend_extras() {
    let descriptor = DeviceDescriptor::new("pluto", "usb:1.6.5", "x");
    let mut cfg = SdrConfig {
        backend_id: "pluto".into(),
        device_id: "usb:1.6.5".into(),
        gain: GainSetting::Manual(ManualGainValue::Db { db: 30 }),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tx_attenuation_db".into(), json!(25.5));
    cfg.backend_extras.insert("prefer_low_rate".into(), json!(false));
    let pcfg = build_pluto_config(&descriptor, &cfg).unwrap();
    assert!((pcfg.tx_attenuation_db - 25.5).abs() < 1e-6);
    assert!(!pcfg.prefer_low_rate);
}
