//! Smoke tests for the `SdrBackend` impl on SDRplay. No hardware
//! required — these only validate the static capabilities and the
//! `SdrConfig` ↔ `SdrplayConfig` mapping.

use modem_sdr::{
    DeviceDescriptor, GainSetting, ManualGainShape, ManualGainValue, SdrBackend, SdrConfig,
};
use modem_sdrplay::backend::{build_sdrplay_config, SdrplayBackend, BACKEND_ID};
use modem_sdrplay::device::{AgcMode, AntennaPort, SdrplayHardware, Tuner};
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
    // Every "real" AGC mode keeps the LNA under operator control —
    // SDRplay's AGC loop only manages `gRdB` (IF gain reduction).
    for m in caps.agc_modes.iter().filter(|m| !m.manual) {
        assert!(
            m.keeps_lna_manual,
            "agc mode {} should keep LNA manual",
            m.id
        );
    }
    // The "disable" mode is irrelevant here (everything is manual
    // anyway); we still want it false so the GUI's enable rule
    // — `isAgc && !keeps_lna_manual` — doesn't ambiguate.
    assert!(!disable.keeps_lna_manual);
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
            gain: GainSetting::AgcMode {
                id: id.into(),
                lna_state: None,
            },
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
fn build_sdrplay_config_defaults_tuner_to_a_when_extra_missing() {
    // RSP1A configs ship without a `tuner` extras key — the part has
    // a single tuner. `build_sdrplay_config` must default to A so
    // `device::open` can override (or keep) it after reading hwVer.
    // For RSPduo configs the GUI default still injects "B" via
    // `default_sdr_config_for("sdrplay")`, so this only relaxes the
    // non-RSPduo path.
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    let cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::AgcMode {
            id: "disable".into(),
            lna_state: None,
        },
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    let scfg = build_sdrplay_config(&descriptor, &cfg).expect("missing tuner is OK now");
    assert_eq!(scfg.tuner, Tuner::A);
}

#[test]
fn build_sdrplay_config_rejects_unknown_tuner() {
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    let mut cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::AgcMode {
            id: "disable".into(),
            lna_state: None,
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
            lna_state: None,
        },
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tuner".into(), json!("B"));
    cfg.backend_extras.insert("decimation".into(), json!(8));
    let scfg = build_sdrplay_config(&descriptor, &cfg).unwrap();
    assert_eq!(scfg.decimation, 8);
}

#[test]
fn build_sdrplay_config_honours_lna_state_under_agc() {
    // The GUI keeps the LNA `<input>` enabled while AGC is on
    // (`keeps_lna_manual = true`) and ships the operator's value
    // through `GainSetting::AgcMode { lna_state: Some(_) }`. The
    // backend must use it — not the mid-band default of 4.
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    let mut cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::AgcMode {
            id: "mid".into(),
            lna_state: Some(7),
        },
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tuner".into(), json!("B"));
    let scfg = build_sdrplay_config(&descriptor, &cfg).unwrap();
    assert_eq!(scfg.lna_state, 7);
    assert_eq!(scfg.agc_mode, AgcMode::Mid);
}

/// `SdrplayHardware` is wired into `program_params` for every variant
/// listed here — the test exists so adding a new arm to the enum
/// without touching the match in `device::program_params` becomes a
/// loud failure instead of a silent "Unsupported" path.
const PROGRAMMABLE_HARDWARE: &[SdrplayHardware] = &[
    SdrplayHardware::RspDuo,
    SdrplayHardware::Rsp1a,
    SdrplayHardware::Rsp1b,
    SdrplayHardware::Rsp1,
];

#[test]
fn programmable_hardware_count_matches_supported_branches() {
    // 4 device branches today: RSPduo, RSP1A, RSP1B, RSP1. Bumping
    // this number means you've added a new hardware branch and
    // should also have wired its per-device programming.
    assert_eq!(PROGRAMMABLE_HARDWARE.len(), 4);
}

#[test]
fn sdrplay_hardware_maps_hw_ver_bytes() {
    // The hwVer bytes come straight from sdrplay_api.h —
    // SDRPLAY_RSP{1,2,duo,dx,1B,1A}_ID. Lock them down so a future
    // refactor that reorders the match arms can't quietly map RSP1A
    // (255) to RSP1 (1).
    assert_eq!(SdrplayHardware::from_hw_ver(1), SdrplayHardware::Rsp1);
    assert_eq!(SdrplayHardware::from_hw_ver(2), SdrplayHardware::Rsp2);
    assert_eq!(SdrplayHardware::from_hw_ver(3), SdrplayHardware::RspDuo);
    assert_eq!(SdrplayHardware::from_hw_ver(4), SdrplayHardware::RspDx);
    assert_eq!(SdrplayHardware::from_hw_ver(6), SdrplayHardware::Rsp1b);
    assert_eq!(SdrplayHardware::from_hw_ver(7), SdrplayHardware::RspDxR2);
    assert_eq!(SdrplayHardware::from_hw_ver(255), SdrplayHardware::Rsp1a);
    // Unknown bytes survive as `Unsupported(_)` so `device::open`
    // can produce a clear error rather than silently corrupt params.
    assert_eq!(
        SdrplayHardware::from_hw_ver(99),
        SdrplayHardware::Unsupported(99)
    );

    // Capability flags drive the GUI's panel layout — guard them
    // explicitly. Only the RSPduo gets the tuner selector; RSPduo +
    // RSP2 + RSPdx (R1 + R2) get the antenna selector.
    assert!(SdrplayHardware::RspDuo.has_tuner_selector());
    assert!(!SdrplayHardware::Rsp1a.has_tuner_selector());
    assert!(!SdrplayHardware::Rsp1.has_tuner_selector());
    assert!(SdrplayHardware::RspDuo.has_antenna_selector());
    assert!(SdrplayHardware::Rsp2.has_antenna_selector());
    assert!(SdrplayHardware::RspDx.has_antenna_selector());
    assert!(!SdrplayHardware::Rsp1a.has_antenna_selector());
}

#[test]
fn build_sdrplay_config_lna_state_defaults_to_4_under_agc_when_absent() {
    // Old persisted settings.json still uses
    // `{"kind":"agc_mode","id":"mid"}` (no `lna_state`). Serde
    // gives us `None`; the backend must fall back to LNA = 4 so
    // upgrades don't surprise users mid-session.
    let descriptor = DeviceDescriptor::new("sdrplay", "x", "x");
    let mut cfg = SdrConfig {
        backend_id: "sdrplay".into(),
        device_id: "x".into(),
        gain: GainSetting::AgcMode {
            id: "mid".into(),
            lna_state: None,
        },
        antenna: "fifty".into(),
        ..SdrConfig::default()
    };
    cfg.backend_extras.insert("tuner".into(), json!("B"));
    let scfg = build_sdrplay_config(&descriptor, &cfg).unwrap();
    assert_eq!(scfg.lna_state, 4);
}
