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
use std::sync::mpsc::{Receiver, Sender};

use crate::telemetry::{RadioCommand, RadioTelemetry};

pub struct SdrCaptureHandle {
    /// Holds the backend's native capture handle alive. `Drop` on
    /// the boxed `Any` cascades through to the inner type's `Drop`
    /// impl — this is how the capture thread learns to stop.
    _inner: Box<dyn Any + Send>,
    /// Radio-tab telemetry stream (RF/audio spectrum, S-meter, tune
    /// state). `Some` only for backends that wired the Radio path via
    /// [`Self::with_radio`]; the soundcard RX path leaves it `None`.
    /// Taken once by the worker via [`Self::take_telemetry`].
    telemetry: Option<Receiver<RadioTelemetry>>,
    /// Control channel into the capture thread (live retune / gain /
    /// squelch). `Some` only with [`Self::with_radio`].
    control: Option<Sender<RadioCommand>>,
}

impl SdrCaptureHandle {
    /// Wrap any backend's native `CaptureHandle` (or equivalent).
    /// The inner type must own its own teardown via `Drop` — the
    /// wrapper offers no other affordance. No Radio telemetry/control.
    pub fn new<T: Any + Send>(handle: T) -> Self {
        Self {
            _inner: Box::new(handle),
            telemetry: None,
            control: None,
        }
    }

    /// Like [`Self::new`] but also carries the Radio-tab telemetry
    /// receiver and the control-command sender wired into the capture
    /// thread. Backends that support the Radio tab build the two
    /// channels, hand the capture thread the opposite ends, and pass
    /// the consumer ends here.
    pub fn with_radio<T: Any + Send>(
        handle: T,
        telemetry: Receiver<RadioTelemetry>,
        control: Sender<RadioCommand>,
    ) -> Self {
        Self {
            _inner: Box::new(handle),
            telemetry: Some(telemetry),
            control: Some(control),
        }
    }

    /// Take the telemetry receiver (once). Returns `None` for non-radio
    /// captures or on a second call.
    pub fn take_telemetry(&mut self) -> Option<Receiver<RadioTelemetry>> {
        self.telemetry.take()
    }

    /// Borrow the control-command sender, if this capture supports the
    /// Radio tab. Clone it to hand a copy to the worker.
    pub fn control(&self) -> Option<&Sender<RadioCommand>> {
        self.control.as_ref()
    }
}
