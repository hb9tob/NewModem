//! Sanity tests on the `modem-sdr` types in isolation.
//!
//! Phase-A gate: this crate compiles and tests pass with no backend
//! impls present. The asserts here protect the trait surface from
//! accidental drift (default trait shape, serialization invariants,
//! enum-variant coverage) — they're called by `cargo test -p
//! modem-sdr`. Once backend crates exist, each will add its own
//! `tests/backend_smoke.rs` to walk the registered impl through
//! the same asserts.

use modem_sdr::{
    AgcMode, AntennaChoice, BackendCapabilities, BackendFeatures, GainSetting, ManualGainShape,
    ManualGainValue, SampleRateStrategy, SdrConfig,
};

/// Hand-rolled `BackendCapabilities` mimicking what a real backend
/// would return. Used by the invariant asserts below.
fn dummy_caps() -> BackendCapabilities {
    BackendCapabilities {
        rx_supported: true,
        tx_supported: true,
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
                label: "Manuel".into(),
                manual: true,
                keeps_lna_manual: false,
            },
            AgcMode {
                id: "fast_attack".into(),
                label: "AGC rapide".into(),
                manual: false,
                keeps_lna_manual: false,
            },
        ],
        antennas: vec![],
        features: BackendFeatures {
            ctcss_tx: true,
            rf_bandwidth_range_hz: Some((200_000, 56_000_000)),
            ..Default::default()
        },
        sample_rate_strategy: SampleRateStrategy {
            host_iq_rate_hz: 576_000,
            audio_decim_ratio: 12,
        },
    }
}

#[test]
fn capabilities_serialize_to_json() {
    // The Tauri layer ships caps to the frontend as JSON. Make
    // sure that round-trip works end-to-end.
    let caps = dummy_caps();
    let json = serde_json::to_string(&caps).expect("caps serialize");
    assert!(json.contains("\"rx_supported\":true"));
    assert!(json.contains("\"tx_supported\":true"));
    assert!(json.contains("\"agc_modes\""));
    assert!(json.contains("\"manual\""));
}

#[test]
fn capabilities_invariants() {
    let caps = dummy_caps();
    // Every backend should support at least one direction.
    assert!(caps.rx_supported || caps.tx_supported);
    // Frequency range present iff direction supported.
    assert_eq!(caps.rx_supported, caps.rx_freq_range_hz.is_some());
    assert_eq!(caps.tx_supported, caps.tx_freq_range_hz.is_some());
    // Frequency ranges have min < max.
    if let Some((lo, hi)) = caps.rx_freq_range_hz {
        assert!(lo < hi, "rx freq range invalid");
    }
    if let Some((lo, hi)) = caps.tx_freq_range_hz {
        assert!(lo < hi, "tx freq range invalid");
    }
    // AGC IDs are unique.
    let mut ids: Vec<&str> = caps.agc_modes.iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    let len = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), len, "duplicate AGC mode IDs");
}

#[test]
fn config_default_round_trips_through_json() {
    // settings.json persists SdrConfig. Default should serialize
    // and deserialize cleanly so a fresh install loads without
    // error.
    let cfg = SdrConfig::default();
    let json = serde_json::to_string(&cfg).expect("config serialize");
    let back: SdrConfig = serde_json::from_str(&json).expect("config deserialize");
    assert_eq!(cfg.rx_freq_hz, back.rx_freq_hz);
    assert_eq!(cfg.max_deviation_hz, back.max_deviation_hz);
    // backend_extras default is empty map.
    assert!(back.backend_extras.is_empty());
}

#[test]
fn gain_setting_variants_round_trip() {
    // The GUI reads/writes GainSetting; guard the variant tags.
    let values = [
        GainSetting::Manual(ManualGainValue::Db { db: 30 }),
        GainSetting::Manual(ManualGainValue::LnaPlusIf {
            lna_state: 4,
            if_grdb: 40,
        }),
        GainSetting::Manual(ManualGainValue::Discrete { step_idx: 7 }),
        GainSetting::AgcMode {
            id: "slow_attack".into(),
            lna_state: None,
        },
        GainSetting::AgcMode {
            id: "mid".into(),
            lna_state: Some(7),
        },
    ];
    for g in values {
        let json = serde_json::to_string(&g).expect("gain serialize");
        let _back: GainSetting = serde_json::from_str(&json).expect("gain deserialize");
    }
}

#[test]
fn legacy_agc_mode_json_without_lna_state_deserializes() {
    // Old persisted GUI settings.json predates the `lna_state`
    // overlay on the AGC variant. Make sure they still load —
    // the missing field defaults to `None` (cf. `#[serde(default)]`).
    let legacy = r#"{"kind":"agc_mode","id":"mid"}"#;
    let g: GainSetting = serde_json::from_str(legacy).expect("legacy gain deserialize");
    match g {
        GainSetting::AgcMode { id, lna_state } => {
            assert_eq!(id, "mid");
            assert!(lna_state.is_none());
        }
        _ => panic!("expected AgcMode variant"),
    }
}

#[test]
fn antenna_choice_serializes() {
    let a = AntennaChoice {
        id: "hiz".into(),
        label: "Hi-Z (1 kHz–60 MHz)".into(),
    };
    let json = serde_json::to_string(&a).unwrap();
    assert!(json.contains("\"hiz\""));
}
