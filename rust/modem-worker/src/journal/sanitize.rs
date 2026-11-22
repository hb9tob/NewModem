//! What is allowed into the journal.
//!
//! Journals are **published** on the collector. The worker events carry
//! paths, file names and free-form error text meant for the local GUI, so
//! nothing is copied by default: each event lists the fields that may be
//! logged, and an unknown event is not logged at all. A field added to a
//! worker payload later stays private until it is listed here.

use serde_json::{Map, Value};

/// Longest string value kept from an event.
const MAX_STR: usize = 200;
/// Longest callsign kept (`F/HB9TOB/P` is 10).
const MAX_CALLSIGN: usize = 16;

/// Loggable fields per event name. An empty list logs the occurrence only,
/// which is what we want for error events whose message may embed a path.
fn allowed_fields(event: &str) -> Option<&'static [&'static str]> {
    Some(match event {
        "modem_state" => &["active", "profile"],
        "preamble" => &["profile", "offset_seconds"],
        "header" => &["profile", "mode_code", "payload_length"],
        "app_header" => &["session_id", "file_size", "mime_type"],
        "envelope" => &["callsign"],
        "session_armed" => &["session_id", "k", "t", "file_size", "mime_type", "profile"],
        "session_progress" => &["session_id", "received", "needed", "decoded", "cap_reached"],
        "session_decoded" => &["session_id", "size", "callsign"],
        "file_complete" => &["callsign", "mime_type", "size", "sigma2", "sigma2_data_avg"],
        "v2_progress" => &[
            "blocks_converged",
            "blocks_total",
            "sigma2",
            "sigma2_data",
            "data_evm",
        ],
        "sf_detail" => &[
            "profile",
            "ppm",
            "residual_ppm",
            "session_ppm",
            "ffe_shift",
            "converged_blocks",
            "total_blocks",
            "eot",
        ],
        "grid_used" => &["duration_ms", "fallback", "n_passes", "drift_ppm"],
        "profile_auto_detected" => &["profile"],
        "v3_profile_detected" => &["from", "to"],
        "worker_requests_restart" => &["reason"],
        "session_end" => &[],
        "error" => &[],
        "tx_plan" => &["duration_s", "total_blocks", "wire_bytes", "mode"],
        "tx_complete" => &["duration_s", "stopped_early"],
        "tx_error" => &[],
        _ => return None,
    })
}

/// Loggable fields per `rx_milestone` stage.
fn milestone_fields(stage: &str) -> &'static [&'static str] {
    match stage {
        "gate_poll" => &["ms", "cfo", "warm_s"],
        "gate_open" => &["family", "metric", "cfo_hz", "preamble_s"],
        "driver_ready" => &["replay_ms", "warm_s", "replayed_s", "active"],
        "driver_drop" => &["reason", "alive_s"],
        "marker" => &[
            "marker_s",
            "cycle_idx",
            "seg_id",
            "base_esi",
            "is_meta",
            "profile_index",
            "first",
            "timing_ppm",
            "drift_ppm",
        ],
        "drift_committed" => &["from_ppm", "to_ppm", "n_obs", "applied"],
        "rewind" => &["anchor_s", "new_drift_ppm"],
        "rewind_replay" => &["span_s", "replay_ms"],
        "rebuild" => &["to", "history_s", "replay_ms"],
        "session_lost" => &["reason"],
        "burst_finalised" => &["cycles", "cws"],
        "brickwall" => &["reason", "dropped"],
        "scan" => &[
            "gated",
            "ok",
            "active",
            "finalize",
            "probe_ms",
            "probe_ratio",
            "decode_ms",
            "n_passes",
            "buf_s",
            "drift_ppm",
            "cws",
            "converged",
            "total_blocks",
            "sigma2",
            "last_preamble_s",
        ],
        // Known or future stages without detail (eot, burst_exit).
        _ => &[],
    }
}

/// Reduce a worker event to what may be published. Returns the record kind
/// and its fields, or `None` when the event is not journalled.
pub fn event_record(name: &str, payload: &Value) -> Option<(String, Map<String, Value>)> {
    let (kind, fields) = if name == "rx_milestone" {
        let stage = payload.get("stage")?.as_str()?;
        (format!("rx.{}", text(stage)), milestone_fields(stage))
    } else {
        let fields = allowed_fields(name)?;
        // `header` is the kind of the journal's own first record.
        let kind = if name == "header" { "rx_header" } else { name };
        (kind.to_string(), fields)
    };
    let mut out = Map::new();
    for &key in fields {
        let Some(value) = payload.get(key) else { continue };
        let clean = match value {
            Value::String(s) if key == "callsign" => {
                let call = callsign(s);
                if call.is_empty() {
                    continue;
                }
                Value::from(call)
            }
            Value::String(s) => Value::from(text(s)),
            Value::Number(_) | Value::Bool(_) => value.clone(),
            // Null adds nothing; arrays and objects are never in the lists.
            _ => continue,
        };
        out.insert(key.to_string(), clean);
    }
    Some((kind, out))
}

/// A callsign received over the air is untrusted input that ends up on a
/// public web page: keep `[A-Z0-9/-]` only.
pub fn callsign(raw: &str) -> String {
    raw.chars()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '/' | '-'))
        .take(MAX_CALLSIGN)
        .collect()
}

/// Printable, single-line, path-free text. Slashes and backslashes are
/// replaced, so no string value can carry a usable path or markup.
pub fn text(raw: &str) -> String {
    raw.chars()
        .take(MAX_STR)
        .map(|c| {
            let ok = c.is_ascii_alphanumeric()
                || matches!(c, ' ' | '+' | '-' | '_' | '.' | ',' | ':' | ';' | '=' | '(' | ')');
            if ok {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decoded_header_cannot_pass_for_the_journal_header() {
        let (kind, fields) =
            event_record("header", &serde_json::json!({"profile": "HighPlus"})).unwrap();
        assert_eq!(kind, "rx_header");
        assert_eq!(fields["profile"], "HighPlus");
    }

    #[test]
    fn paths_and_file_names_never_pass() {
        let events = [
            (
                "file_complete",
                json!({"filename": "holiday.avif", "saved_path": "/home/op/holiday.avif",
                       "callsign": "HB9TOB", "size": 12}),
            ),
            (
                "session_decoded",
                json!({"session_dir": "/home/op/s/1", "decoded_path": "/home/op/s/1/d",
                       "filename": "holiday.avif", "session_id": 7}),
            ),
            (
                "session_armed",
                json!({"session_dir": "C:\\Users\\op\\s", "k": 93}),
            ),
            ("envelope", json!({"filename": "holiday.avif", "callsign": "F4ABC"})),
            ("error", json!({"message": "save failed: /home/op/holiday.avif"})),
            ("tx_error", json!({"message": "read /home/op/holiday.avif"})),
            ("tx_plan", json!({"wav_path": "/home/op/tx.wav", "filename": "holiday.avif"})),
        ];
        for (name, payload) in events {
            let (_, fields) = event_record(name, &payload).expect(name);
            let line = serde_json::to_string(&fields).unwrap();
            for leak in ["/home", "Users", "holiday", "message", "_path", "_dir"] {
                assert!(!line.contains(leak), "{name} leaked {leak}: {line}");
            }
        }
    }

    #[test]
    fn unknown_events_are_not_logged() {
        assert!(event_record("radio_spectrum", &json!({"bins": [1, 2]})).is_none());
        assert!(event_record("audio_level", &json!({"rms": 0.1})).is_none());
    }

    #[test]
    fn milestone_keeps_listed_fields_only() {
        let payload = json!({"stage": "gate_open", "metric": 0.31, "family": "HIGH+",
                             "secret": "x", "_wall_s": 1.0});
        let (kind, fields) = event_record("rx_milestone", &payload).unwrap();
        assert_eq!(kind, "rx.gate_open");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields["family"], "HIGH+");
    }

    #[test]
    fn arrays_are_dropped_even_under_an_allowed_key() {
        let (_, fields) = event_record("v2_progress", &json!({"sigma2": [1.0, 2.0]})).unwrap();
        assert!(fields.is_empty());
    }

    #[test]
    fn callsigns_from_the_air_are_reduced() {
        assert_eq!(callsign("f/hb9tob/p"), "F/HB9TOB/P");
        assert_eq!(callsign("<img src=x>"), "IMGSRCX");
        assert_eq!(callsign("../../ETC"), "//ETC");
        assert_eq!(callsign(&"A".repeat(80)).len(), MAX_CALLSIGN);
    }

    #[test]
    fn text_has_no_separator_or_markup() {
        assert_eq!(text("/home/op\n<b>"), "_home_op__b_");
        assert_eq!(text("lost sync: 3 (pred=12)"), "lost sync: 3 (pred=12)");
    }
}
