//! What one RX worker is doing, and how much audio waits in each buffer.
//!
//! The worker publishes into atomics; the journal's 1 Hz thread reads them.
//! The record is therefore still written while the worker is stuck in a
//! long replay, which is exactly when it matters.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::cpu;
use super::queue::QueueMeter;
use super::Journal;

const SAMPLES_PER_MS: f64 = modem_core::types::AUDIO_RATE as f64 / 1000.0;

/// Activities at least this long get their own record.
const SLOW_ACTIVITY: Duration = Duration::from_millis(50);

/// 48 kHz sample count as milliseconds of audio, 0.1 ms resolution.
pub fn samples_ms(samples: u64) -> f64 {
    (samples as f64 / SAMPLES_PER_MS * 10.0).round() / 10.0
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

/// What the worker thread can be doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Wait,
    Gate,
    Push,
    Scan,
    ReplayWarm,
    ReplayRebuild,
    ReplayRewind,
    Finalize,
    Save,
}

impl Activity {
    const ALL: [Activity; 9] = [
        Self::Wait,
        Self::Gate,
        Self::Push,
        Self::Scan,
        Self::ReplayWarm,
        Self::ReplayRebuild,
        Self::ReplayRewind,
        Self::Finalize,
        Self::Save,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Gate => "gate",
            Self::Push => "push",
            Self::Scan => "scan",
            Self::ReplayWarm => "replay_warm",
            Self::ReplayRebuild => "replay_rebuild",
            Self::ReplayRewind => "replay_rewind",
            Self::Finalize => "finalize",
            Self::Save => "save",
        }
    }
}

/// Buffers downstream of the worker queue, published in samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffer {
    /// Chunk taken from the queue and being processed.
    InFlight,
    /// Turbo warm-up ring (cold detection).
    Warm,
    /// Legacy sliding window, or the turbo session's rolling buffer.
    Session,
    /// Turbo replay history (what a rebuild would replay).
    History,
}

impl Buffer {
    const ALL: [Buffer; 4] = [Self::InFlight, Self::Warm, Self::Session, Self::History];

    fn key(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight_ms",
            Self::Warm => "warm_ms",
            Self::Session => "session_ms",
            Self::History => "history_ms",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Stat {
    count: u32,
    wall: Duration,
    /// Thread CPU; excludes the legacy worker's scoped helper threads.
    cpu: Duration,
    max: Duration,
}

/// Shared state of one worker run.
pub struct RunProbe {
    run: u32,
    journal: Journal,
    meter: Arc<QueueMeter>,
    started: Instant,
    activity: AtomicU8,
    activity_since_us: AtomicU64,
    buffers: [AtomicU64; Buffer::ALL.len()],
    stats: Mutex<[Stat; Activity::ALL.len()]>,
}

impl RunProbe {
    pub fn new(run: u32, journal: Journal, meter: Arc<QueueMeter>) -> Arc<Self> {
        Arc::new(Self {
            run,
            journal,
            meter,
            started: Instant::now(),
            activity: AtomicU8::new(Activity::Wait as u8),
            activity_since_us: AtomicU64::new(0),
            buffers: Default::default(),
            stats: Mutex::new(Default::default()),
        })
    }

    pub fn run(&self) -> u32 {
        self.run
    }

    /// Audio waiting in the worker queue right now.
    pub fn queue_ms(&self) -> f64 {
        samples_ms(self.meter.queued())
    }

    pub fn set_buffer(&self, buffer: Buffer, samples: usize) {
        self.buffers[buffer as usize].store(samples as u64, Ordering::Relaxed);
    }

    /// Mark the worker as doing `activity` until the guard drops. Nests: a
    /// rebuild replay inside a push restores `push` when it ends, and is
    /// counted in both (totals are inclusive).
    pub fn enter(self: &Arc<Self>, activity: Activity) -> ActivityGuard {
        let previous = self.activity.swap(activity as u8, Ordering::Relaxed);
        let previous_since_us = self
            .activity_since_us
            .swap(self.started.elapsed().as_micros() as u64, Ordering::Relaxed);
        ActivityGuard {
            probe: self.clone(),
            activity,
            previous,
            previous_since_us,
            queue_before: self.meter.queued(),
            wall: Instant::now(),
            cpu: cpu::thread_cpu(),
        }
    }

    /// Fields of the periodic `buffer` record. Resets the per-window values.
    pub fn sample(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("queue_ms".into(), self.queue_ms().into());
        out.insert("queue_max_ms".into(), samples_ms(self.meter.take_peak()).into());
        out.insert("captured_ms".into(), samples_ms(self.meter.produced()).into());
        out.insert("consumed_ms".into(), samples_ms(self.meter.consumed()).into());
        out.insert("run_wall_ms".into(), ms(self.started.elapsed()).into());
        for buffer in Buffer::ALL {
            let n = self.buffers[buffer as usize].load(Ordering::Relaxed);
            if n > 0 {
                out.insert(buffer.key().into(), samples_ms(n).into());
            }
        }

        let doing = Activity::ALL[self.activity.load(Ordering::Relaxed) as usize];
        let since = Duration::from_micros(self.activity_since_us.load(Ordering::Relaxed));
        out.insert("doing".into(), doing.name().into());
        out.insert("doing_ms".into(), ms(self.started.elapsed().saturating_sub(since)).into());

        let stats = std::mem::take(&mut *self.stats.lock().unwrap_or_else(PoisonError::into_inner));
        let mut spent = Map::new();
        for (activity, st) in Activity::ALL.iter().zip(stats) {
            if st.count > 0 {
                // [count, wall ms, thread CPU ms, longest ms]
                spent.insert(
                    activity.name().into(),
                    json!([st.count, ms(st.wall), ms(st.cpu), ms(st.max)]),
                );
            }
        }
        if !spent.is_empty() {
            out.insert("spent".into(), Value::Object(spent));
        }
        out
    }
}

/// Ends an activity when dropped. See [`RunProbe::enter`].
pub struct ActivityGuard {
    probe: Arc<RunProbe>,
    activity: Activity,
    previous: u8,
    previous_since_us: u64,
    queue_before: u64,
    wall: Instant,
    cpu: Option<Duration>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let wall = self.wall.elapsed();
        let spent_cpu = self
            .cpu
            .zip(cpu::thread_cpu())
            .map(|(before, after)| after.saturating_sub(before));
        {
            let mut stats = self.probe.stats.lock().unwrap_or_else(PoisonError::into_inner);
            let st = &mut stats[self.activity as usize];
            st.count += 1;
            st.wall += wall;
            st.cpu += spent_cpu.unwrap_or_default();
            st.max = st.max.max(wall);
        }
        self.probe.activity.store(self.previous, Ordering::Relaxed);
        self.probe
            .activity_since_us
            .store(self.previous_since_us, Ordering::Relaxed);

        // A long wait is an idle channel, not an event.
        if wall >= SLOW_ACTIVITY && self.activity != Activity::Wait {
            let mut f = Map::new();
            f.insert("what".into(), self.activity.name().into());
            f.insert("ms".into(), ms(wall).into());
            if let Some(c) = spent_cpu {
                f.insert("cpu_ms".into(), ms(c).into());
            }
            f.insert("queue_before_ms".into(), samples_ms(self.queue_before).into());
            f.insert("queue_ms".into(), self.probe.queue_ms().into());
            self.probe.journal.record("activity", Some(self.probe.run), f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::MeteredReceiver;
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn two_second_stall_is_visible_during_the_stall() {
        let (tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        let probe = RunProbe::new(1, Journal::disabled(), metered.meter());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let worker_probe = probe.clone();
        let worker = std::thread::spawn(move || {
            let _guard = worker_probe.enter(Activity::ReplayRebuild);
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            // Nothing is consumed until the stall ends.
            drop(metered);
        });
        entered_rx.recv().unwrap();
        for _ in 0..100 {
            tx.send(vec![0.0f32; 960]).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while probe.queue_ms() < 2000.0 {
            assert!(Instant::now() < deadline, "backlog never became visible");
            std::thread::sleep(Duration::from_millis(5));
        }
        let during = probe.sample();
        assert_eq!(during["queue_ms"], 2000.0);
        assert_eq!(during["doing"], "replay_rebuild");
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(probe.sample()["spent"]["replay_rebuild"].is_array());
    }

    #[test]
    fn nested_activity_restores_the_outer_one() {
        let (_tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        let probe = RunProbe::new(1, Journal::disabled(), metered.meter());
        let outer = probe.enter(Activity::Push);
        drop(probe.enter(Activity::ReplayRewind));
        assert_eq!(probe.sample()["doing"], "push");
        drop(outer);
        assert_eq!(probe.sample()["doing"], "wait");
    }

    #[test]
    fn activity_table_matches_discriminants() {
        for (i, a) in Activity::ALL.iter().enumerate() {
            assert_eq!(*a as usize, i);
        }
        for (i, b) in Buffer::ALL.iter().enumerate() {
            assert_eq!(*b as usize, i);
        }
    }
}
