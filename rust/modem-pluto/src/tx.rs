//! Pluto TX path: 48 kHz audio `Vec<f32>` → USB I/Q at 528 / 960 kSa/s.
//!
//! Implements [`modem_io::traits::SampleSink`] so `modem_worker::tx_worker`
//! can swap a Pluto in for the cpal sink with no logic change. The
//! upsampling + PM modulation happens **inside** the sink on the
//! capture thread (same pattern the plan documents for the streaming
//! TX case): the worker hands over a single 48 kHz `Vec<f32>` and the
//! sink does its own staged push to libiio's TX buffer.
//!
//! Scaffold pass — the trait impl lands in task #11 along with the
//! libiio buffer push loop. The skeleton here pins the type shape so
//! `modem-cli` / `modem-gui` can already wire `Box<dyn SampleSink>`
//! through the `--pluto` code path.
//!
//! Final shape (target, task #11):
//!
//! ```text
//! Vec<f32> @ 48 kHz
//!   → PolyphaseInterpolator (×N, taps from modem-sdr-dsp)
//!   → PhaseMod (radio-faithful PM TX, +6 dB/oct preemph for free)
//!   → pack to S16/16 LE, interleaved I/Q
//!   → libiio cf-ad9361-dds-core-lpc TX buffer
//! ```

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use crate::device::PlutoConfig;
use crate::error::PlutoError;

/// `SampleSink` implementation for the Pluto TX path.
///
/// Carries a [`PlutoConfig`] so the worker only needs the device name
/// at `play_buffer` time — center freq / gain were committed at
/// construction. Cloneable because the GUI may want to keep a handle
/// for reconfigure (retune freq, change attenuation) without rebuilding
/// the libiio context.
#[derive(Clone, Debug)]
pub struct PlutoSink {
    pub config: PlutoConfig,
}

impl PlutoSink {
    pub fn new(config: PlutoConfig) -> Self {
        Self { config }
    }
}

// `SampleSink` lives in `modem-io`; pulling that crate as a dependency
// would couple every workspace consumer of `modem-pluto` to cpal. We
// keep the impl inline here behind a feature-less re-export so the
// scaffold compiles standalone, and add the actual `impl SampleSink
// for PlutoSink` in task #11 once the libiio buffer push exists.
//
// To avoid a dependency cycle (modem-io → modem-pluto → modem-io would
// be an awkward edge in the workspace graph), the `impl SampleSink`
// will live in modem-worker or in a thin glue module — final placement
// decided when the integration lands.

/// One-shot transmission state, updated by the libiio push loop.
/// Mirrors the `pos` counter in `modem_io::traits::PlaybackHandle` so
/// the worker's progress polling code stays unchanged.
///
/// **Scaffold**: type body kept minimal; task #11 grows it with the
/// libiio buffer handle, the worker JoinHandle, and the stop flag.
#[non_exhaustive]
pub struct TxJob {
    pub pos: Arc<AtomicUsize>,
    pub total_samples: usize,
}

/// Build a Pluto TX job — interpolate, PM-modulate, push to libiio.
///
/// **Scaffold**: returns [`PlutoError::NotImplemented`].
pub fn submit(_sink: &PlutoSink, _samples: Vec<f32>) -> Result<TxJob, PlutoError> {
    Err(PlutoError::NotImplemented("tx::submit"))
}
