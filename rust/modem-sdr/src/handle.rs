//! Type-erased RX capture handle returned by `SdrDevice::start_rx`.
//!
//! A backend-agnostic substitute for the various per-backend
//! `CaptureHandle` types (`modem_pluto::rx::CaptureHandle`,
//! `modem_sdrplay::rx::CaptureHandle`, …). Wrapping them in
//! `Box<dyn Any + Send>` lets the GUI store one shape per RX
//! session — the inner type's `Drop` impl handles teardown
//! (sets the stop flag, joins the capture thread, calls
//! `sdrplay_api_Uninit`, …) so we never need an explicit
//! `stop()` method on this wrapper.
//!
//! Why not reuse `modem_io::PlaybackHandle`? PlaybackHandle is
//! shaped for TX progress polling (`pos`, `is_done`) and is
//! `!Send` on Windows because cpal's TX `Stream` is `!Send`. RX
//! handles are inherently `Send` (they own a capture thread that
//! ships samples through a `Sender<Vec<f32>>` — same shape on
//! every platform). Keeping the two paths separate also keeps
//! the `Drop` semantics distinct: PlaybackHandle's drop kills
//! the cpal stream, ours kills a capture thread.

use std::any::Any;

pub struct SdrCaptureHandle {
    /// Holds the backend's native capture handle alive. `Drop` on
    /// the boxed `Any` cascades through to the inner type's `Drop`
    /// impl — this is how the capture thread learns to stop.
    _inner: Box<dyn Any + Send>,
}

impl SdrCaptureHandle {
    /// Wrap any backend's native `CaptureHandle` (or equivalent).
    /// The inner type must own its own teardown via `Drop` — the
    /// wrapper offers no other affordance.
    pub fn new<T: Any + Send>(handle: T) -> Self {
        Self {
            _inner: Box::new(handle),
        }
    }
}
