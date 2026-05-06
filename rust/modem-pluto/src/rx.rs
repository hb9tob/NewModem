//! Pluto RX path: USB I/Q → 48 kHz mono `Vec<f32>` audio batches.
//!
//! Mirrors the shape of `modem_io::cpal_capture` so `modem-worker` can
//! plug a Pluto into the same `Receiver<Vec<f32>>` consumer it already
//! uses for the soundcard. Scaffold pass — the capture thread shell is
//! sketched out so task #11 only has to fill in the libiio buffer
//! pump and the DSP chain plumbing.
//!
//! Final shape (target, task #11):
//!
//! ```text
//! libiio buffer (cf-ad9361-lpc, S12/16 LE, interleaved I/Q)
//!   → demux + scale to Complex<f32>
//!   → PolyphaseDecimator (×N, taps from modem-sdr-dsp)
//!   → QuadratureDemod
//!   → DeemphasisLpf (radio-faithful, GR fm_deemph 300 Hz corner)
//!   → SubAudioHpf  (mirrors radio's CTCSS-reject)
//!   → mpsc::Sender<Vec<f32>> → rx_worker
//! ```

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::device::PlutoConfig;
use crate::error::PlutoError;

/// Live handle on a Pluto capture thread. Drop to stop streaming.
///
/// Same shape as `modem_io::cpal_capture::CaptureHandle` — the only
/// difference is `sample_rate` always ends up at 48 kHz here (the
/// decimation chain's output), with `negotiated_iq_rate_hz` exposing
/// what was actually programmed on the AD9363 for diagnostics.
pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    pub sample_rate: u32,
    pub channels: u16,
    /// AD9363 sample rate that the BBPLL locked at — 528 kHz typically,
    /// 960 kHz on fallback. Reported for the GUI / CLI.
    pub negotiated_iq_rate_hz: u64,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Open a Pluto for receive, kick off the capture thread, and return a
/// channel of 48 kHz mono f32 batches.
///
/// **Scaffold**: returns [`PlutoError::NotImplemented`]. Task #11
/// fills in the libiio buffer loop, the DSP chain, and the throttled
/// error log (the latter mirrored from
/// `modem_io::cpal_capture::run_capture` — see commit bab4311).
pub fn start(_config: &PlutoConfig) -> Result<(CaptureHandle, Receiver<Vec<f32>>), PlutoError> {
    let (_tx, _rx) = mpsc::channel::<Vec<f32>>();
    Err(PlutoError::NotImplemented("rx::start"))
}
