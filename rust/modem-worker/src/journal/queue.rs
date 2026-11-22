//! Exact occupancy of the capture → RX worker sample queue.
//!
//! `lag_ms` (wall clock − audio consumed) is not a buffer measurement: it
//! integrates the sound-card clock drift and miscounts discarded chunks.
//! Here every sample is counted when it enters the worker queue and when
//! the worker takes it out, so `queued` is the audio actually waiting, and
//! it can be read by another thread while the worker is busy (a 2 s replay
//! is visible *during* the replay).
//!
//! The relay thread makes this source-agnostic (cpal, ALSA-direct, SDR,
//! WAV pacer): no backend has to count for itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Shared counters of one worker queue. All values are 48 kHz mono samples.
#[derive(Debug, Default)]
pub struct QueueMeter {
    produced: AtomicU64,
    consumed: AtomicU64,
    /// Highest occupancy since the last `take_peak`.
    peak: AtomicU64,
}

impl QueueMeter {
    /// Samples delivered by the capture so far.
    pub fn produced(&self) -> u64 {
        self.produced.load(Ordering::Acquire)
    }

    /// Samples taken out of the queue by the worker so far (decoded or not).
    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Acquire)
    }

    /// Samples currently waiting in the queue.
    pub fn queued(&self) -> u64 {
        // `consumed` first: it can only lag behind the `produced` read after
        // it, so the difference cannot underflow.
        let consumed = self.consumed();
        self.produced().saturating_sub(consumed)
    }

    /// Highest occupancy since the previous call (at least the current one).
    pub fn take_peak(&self) -> u64 {
        let now = self.queued();
        self.peak.swap(now, Ordering::AcqRel).max(now)
    }

    fn on_produced(&self, n: u64) {
        let produced = self.produced.fetch_add(n, Ordering::AcqRel) + n;
        let queued = produced.saturating_sub(self.consumed());
        self.peak.fetch_max(queued, Ordering::AcqRel);
    }

    fn on_consumed(&self, n: u64) {
        self.consumed.fetch_add(n, Ordering::AcqRel);
    }
}

/// The worker's end of a metered queue. Same receive semantics as the
/// `mpsc::Receiver` it replaces.
pub struct MeteredReceiver {
    inner: Receiver<Vec<f32>>,
    meter: Arc<QueueMeter>,
}

impl MeteredReceiver {
    /// Interpose the meter on `source`. The relay thread exits as soon as
    /// the source disconnects, or on the first chunk after the worker hung up.
    pub fn new(source: Receiver<Vec<f32>>) -> Self {
        let meter = Arc::new(QueueMeter::default());
        let (tx, inner) = mpsc::channel::<Vec<f32>>();
        let relay_meter = meter.clone();
        thread::spawn(move || {
            while let Ok(chunk) = source.recv() {
                // Count before sending, otherwise the worker could consume a
                // chunk that has not been produced yet.
                relay_meter.on_produced(chunk.len() as u64);
                if tx.send(chunk).is_err() {
                    break;
                }
            }
        });
        Self { inner, meter }
    }

    pub fn meter(&self) -> Arc<QueueMeter> {
        self.meter.clone()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<f32>, RecvTimeoutError> {
        let chunk = self.inner.recv_timeout(timeout)?;
        self.meter.on_consumed(chunk.len() as u64);
        Ok(chunk)
    }

    pub fn try_recv(&self) -> Result<Vec<f32>, TryRecvError> {
        let chunk = self.inner.try_recv()?;
        self.meter.on_consumed(chunk.len() as u64);
        Ok(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "condition never reached");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn backlog_is_visible_while_consumer_is_stalled() {
        let (tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        let meter = metered.meter();
        // 2 s of audio delivered while the worker takes nothing out.
        for _ in 0..100 {
            tx.send(vec![0.0f32; 960]).unwrap();
        }
        wait_until(|| meter.queued() == 96_000);
        assert_eq!(meter.take_peak(), 96_000);
    }

    #[test]
    fn drain_brings_occupancy_to_zero() {
        let (tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        let meter = metered.meter();
        for n in [480usize, 1, 24_000, 7] {
            tx.send(vec![0.0f32; n]).unwrap();
        }
        wait_until(|| meter.queued() == 24_488);
        while metered.try_recv().is_ok() {}
        assert_eq!(meter.queued(), 0);
        assert_eq!(meter.consumed(), meter.produced());
    }

    #[test]
    fn peak_survives_drain_then_resets() {
        let (tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        let meter = metered.meter();
        tx.send(vec![0.0f32; 4800]).unwrap();
        wait_until(|| meter.queued() == 4800);
        metered.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(meter.take_peak(), 4800);
        assert_eq!(meter.take_peak(), 0);
    }

    #[test]
    fn preserves_order_and_propagates_disconnect() {
        let (tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        for v in 0..10 {
            tx.send(vec![v as f32; 3]).unwrap();
        }
        drop(tx);
        for v in 0..10 {
            let chunk = metered.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(chunk, vec![v as f32; 3]);
        }
        assert_eq!(
            metered.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn dropped_worker_releases_the_source() {
        let (tx, rx) = mpsc::channel();
        drop(MeteredReceiver::new(rx));
        // First chunk makes the relay notice, then the source sees the hang-up.
        wait_until(|| tx.send(vec![0.0f32; 8]).is_err());
    }
}
