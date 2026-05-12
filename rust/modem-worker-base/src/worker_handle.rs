//! Shared worker join handle.
//!
//! Both the V3 (`modem-worker::rx_worker`) and the V4 (`modem-worker2x::rx_worker2x`)
//! spawn routines hand the caller back this exact shape. The GUI's
//! `CaptureSession` and `tx_handle` slot keep one and drop it (calling
//! [`WorkerHandle::stop`]) on stop / shutdown.
//!
//! Cancellation contract: setting `stop` to `true` is observed by the
//! worker loop's `Ordering::Relaxed` check at every batch boundary; the
//! thread then drains in-flight work and exits. Joining is best-effort
//! — a panicking thread produces `Err(_)` which we silently swallow,
//! matching the V3 worker's historical behaviour.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Shared cancellation token + worker thread join handle.
pub struct WorkerHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// Signal the worker thread to stop and wait for it to exit. Consumes
    /// the handle: there is exactly one cancellation per worker.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}
