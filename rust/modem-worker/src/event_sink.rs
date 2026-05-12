//! Back-compat shim: the real `EventSink` / `EventSinkExt` / `NoopSink`
//! / `RecordingSink` live in [`modem_worker_base::event_sink`] now.
//! This file just re-exports them so existing `use modem_worker::event_sink::…`
//! and `use modem_worker::{EventSink, EventSinkExt, NoopSink, RecordingSink}`
//! call sites keep compiling unchanged.

pub use modem_worker_base::event_sink::{EventSink, EventSinkExt, NoopSink, RecordingSink};
