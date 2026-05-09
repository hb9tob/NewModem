//! `modem_io::SampleSink` adapter for [`crate::tx::PlutoSink`].
//!
//! The TX worker drives every backend through `Arc<dyn SampleSink>`.
//! Pluto's existing inherent method [`crate::tx::PlutoSink::play_buffer`]
//! takes only a `Vec<f32>` (URI + rate already baked into the
//! [`PlutoConfig`] held by the sink), whereas
//! [`modem_io::SampleSink::play_buffer`] is the cpal-shaped
//! `(device_name, sample_rate, samples)` trio. This module bridges
//! the two: the trait method ignores `device_name`, asserts
//! `sample_rate == 48_000`, calls the inherent method, and wraps the
//! returned [`crate::tx::TxJob`] into a [`modem_io::PlaybackHandle`]
//! whose progress fields point at the same `Arc<AtomicUsize>` —
//! polling stays cheap, the job's existing `Drop` impl is what
//! actually stops TX when the handle is dropped.
//!
//! Removing the worker-internal `trait TxStream` bridge that lives
//! in `modem-worker/src/tx_worker.rs` is the downstream payoff: once
//! Phase E flips the call sites, both Pluto and cpal share one
//! polling loop on `PlaybackHandle`.

use modem_io::{IoError, PlaybackHandle, SampleSink};

use crate::tx::PlutoSink;

impl SampleSink for PlutoSink {
    fn play_buffer(
        &self,
        _device_name: &str,
        sample_rate: u32,
        samples: Vec<f32>,
    ) -> Result<PlaybackHandle, IoError> {
        // The Pluto already knows its libiio URI + IF rate from the
        // config it was built with — `device_name` is meaningless
        // here (we keep it in the signature to match the cpal sink).
        // `sample_rate` IS meaningful: the chain assumes 48 kHz
        // throughout. Anything else is an upstream bug; defend with
        // a clean error rather than silently distorting.
        if sample_rate != modem_sdr_dsp::AUDIO_RATE {
            return Err(IoError::UnsupportedSampleRate {
                device: "pluto".to_string(),
                rate: sample_rate,
            });
        }
        let total = samples.len();
        // Inherent method, NOT the trait method (different arity, no
        // ambiguity at resolution).
        let job = PlutoSink::play_buffer(self, samples)
            .map_err(|e| IoError::Backend(format!("Pluto TX: {e}")))?;
        // Hand the same `pos` Arc that the TX worker thread updates
        // straight to PlaybackHandle — `pos()`/`is_done()` will read
        // the live counter without any extra atomic.
        let pos = job.pos.clone();
        // Drop on PlaybackHandle's `_stream: Box<dyn Any>` cascades
        // to TxJob's existing Drop (sets stop flag, joins worker).
        Ok(PlaybackHandle::new(pos, total, Box::new(job)))
    }
}
