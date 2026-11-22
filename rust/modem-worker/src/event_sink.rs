//! Event sink abstraction — replaces direct `tauri::AppHandle.emit()`
//! calls inside the workers with something the workers can be tested
//! against and the future RX CLI can implement on its own terms.
//!
//! Design notes:
//!
//! - The core trait `EventSink` exposes a single dyn-safe method,
//!   `emit_json(name, payload)`. The signature matches Tauri's
//!   `Emitter::emit` 1:1 (a string event name + a JSON value), so the
//!   migration in phase 2B is a near-mechanical search/replace.
//! - The `EventSinkExt` extension trait adds the ergonomic generic
//!   `emit(name, &payload)` that accepts any `Serialize`. It can't live
//!   on the base trait because generic methods make the trait
//!   non-dyn-safe; an extension trait with a blanket impl gives us the
//!   convenience without losing `Box<dyn EventSink>`.
//! - Two reference implementations ship out of the box:
//!   * `NoopSink`: black-hole, used when a worker is driven for its
//!     side effects (writing files) and event delivery is irrelevant.
//!   * `RecordingSink`: keeps every emission in memory; lets unit tests
//!     assert what a worker emitted without standing up a Tauri app.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

/// Receiver of asynchronous events emitted by a worker.
///
/// Implementations must be `Send + Sync` so workers running on their own
/// thread can hand them across thread boundaries via `Arc`.
pub trait EventSink: Send + Sync {
    /// Emit an event with a string name and a JSON payload. The payload
    /// shape per event name is documented inside the worker that emits it
    /// (cf. phases 2B/2C). Implementations must not panic — e.g. a Tauri
    /// adapter swallows transport errors silently the way `app.emit()`
    /// already does.
    fn emit_json(&self, name: &str, payload: Value);
}

/// Convenience extension that mirrors the shape of `tauri::Emitter::emit`,
/// so call sites read the same way they did before extraction — pass the
/// payload by value, the sink handles serialization.
///
/// Implemented for every `EventSink` via a blanket impl, including
/// `Box<dyn EventSink>` and `Arc<dyn EventSink>`.
pub trait EventSinkExt: EventSink {
    /// Serialize `payload` and forward to `emit_json`. A serialization
    /// failure (which should not happen for the worker's payload types)
    /// degrades to a `null` payload rather than dropping the event.
    fn emit<S: Serialize>(&self, name: &str, payload: S) {
        let value = serde_json::to_value(payload).unwrap_or(Value::Null);
        self.emit_json(name, value);
    }
}

impl<T: EventSink + ?Sized> EventSinkExt for T {}

/// Black-hole sink — silently discards every event.
#[derive(Default, Debug, Clone, Copy)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit_json(&self, _name: &str, _payload: Value) {}
}

/// Wrapper that stamps every object payload with the worker's timing
/// context before forwarding it:
///
/// - `_audio_s`: audio consumed by the worker so far (stream position,
///   seconds), read from the shared `stream_pos` sample counter the worker
///   bumps as it ingests audio;
/// - `_wall_s`: wall-clock seconds since the wrapper was created.
///
/// `_wall_s - _audio_s` is the worker's real-time lag at the instant the
/// event fired, so every milestone (gate open, first marker, session armed,
/// each new ESI...) carries its own lag without per-event bookkeeping.
/// Non-object payloads are forwarded untouched. Extra fields are ignored by
/// the GUI listeners, which read named fields only.
pub struct StampingSink {
    inner: Arc<dyn EventSink>,
    stream_pos: Arc<AtomicU64>,
    sample_rate: f64,
    started: Instant,
}

impl StampingSink {
    pub fn new(inner: Arc<dyn EventSink>, stream_pos: Arc<AtomicU64>, sample_rate: f64) -> Self {
        Self {
            inner,
            stream_pos,
            sample_rate,
            started: Instant::now(),
        }
    }
}

impl EventSink for StampingSink {
    fn emit_json(&self, name: &str, mut payload: Value) {
        if let Value::Object(ref mut m) = payload {
            let audio_s = self.stream_pos.load(Ordering::Relaxed) as f64 / self.sample_rate;
            m.insert("_audio_s".into(), Value::from(audio_s));
            m.insert(
                "_wall_s".into(),
                Value::from(self.started.elapsed().as_secs_f64()),
            );
        }
        self.inner.emit_json(name, payload);
    }
}

/// In-memory sink useful for unit tests: collects every (name, payload)
/// pair so the test can assert what a worker emitted.
#[derive(Default, Debug)]
pub struct RecordingSink {
    events: Mutex<Vec<(String, Value)>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every event seen so far, in emission order.
    pub fn events(&self) -> Vec<(String, Value)> {
        self.events.lock().expect("RecordingSink mutex poisoned").clone()
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("RecordingSink mutex poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl EventSink for RecordingSink {
    fn emit_json(&self, name: &str, payload: Value) {
        self.events
            .lock()
            .expect("RecordingSink mutex poisoned")
            .push((name.to_string(), payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct DummyPayload {
        n: u32,
        msg: &'static str,
    }

    #[test]
    fn noop_drops_events() {
        let sink = NoopSink;
        sink.emit_json("anything", Value::Null);
        sink.emit("typed", DummyPayload { n: 1, msg: "hi" });
    }

    #[test]
    fn recording_captures_events_in_order() {
        let sink = RecordingSink::new();
        sink.emit("first", DummyPayload { n: 1, msg: "a" });
        sink.emit_json("second", serde_json::json!({"x": 42}));
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "first");
        assert_eq!(events[0].1["n"], 1);
        assert_eq!(events[0].1["msg"], "a");
        assert_eq!(events[1].0, "second");
        assert_eq!(events[1].1["x"], 42);
    }

    #[test]
    fn dyn_event_sink_is_object_safe() {
        let sink: Box<dyn EventSink> = Box::new(RecordingSink::new());
        // Both methods callable through the trait object.
        sink.emit_json("a", Value::Null);
        sink.emit("b", DummyPayload { n: 2, msg: "x" });
    }

    #[test]
    fn stamping_sink_adds_stream_position() {
        let rec = Arc::new(RecordingSink::new());
        let pos = Arc::new(AtomicU64::new(0));
        let sink = StampingSink::new(rec.clone(), pos.clone(), 48_000.0);
        pos.store(96_000, Ordering::Relaxed);
        sink.emit("obj", DummyPayload { n: 3, msg: "x" });
        sink.emit_json("null", Value::Null);
        let ev = rec.events();
        assert_eq!(ev[0].1["_audio_s"].as_f64(), Some(2.0));
        assert!(ev[0].1["_wall_s"].as_f64().unwrap() >= 0.0);
        assert_eq!(ev[0].1["n"], 3);
        assert!(ev[1].1.is_null());
    }

    #[test]
    fn serialization_failure_emits_null_payload() {
        // A type that fails serialization through serde_json. Custom
        // Serialize impl that always errors lets us prove the extension
        // method degrades to a null payload instead of panicking.
        struct AlwaysFails;
        impl Serialize for AlwaysFails {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("nope"))
            }
        }
        let sink = RecordingSink::new();
        sink.emit("fail", AlwaysFails);
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "fail");
        assert!(events[0].1.is_null());
    }
}
