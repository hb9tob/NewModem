//! Modem journal: what the modem does over time, how much audio waits in
//! each buffer, and what encoding costs the receiver.
//!
//! ```text
//! capture ─► produced ─► mpsc queue ─► consumed ─► RX worker
//!                 └──── 1 Hz probe, independent of the worker ────┘
//! events + probe + encode ops ─► try_send ─► writer thread ─► JSON lines
//! ```
//!
//! Rules this module keeps:
//!
//! - **It never blocks the modem.** Records go through a bounded channel
//!   with `try_send`; losses are counted and reported in the journal.
//! - **It is publishable.** Worker events pass through [`sanitize`]; callers
//!   of [`Journal::record`] must not put paths, file names or URLs in fields.
//! - **It changes nothing.** No DSP or scheduling decision reads it.
//!
//! Every line has `t_ms` (monotonic, since the journal opened) and `kind`;
//! lines about a worker also have `run` and `queue_ms`.
//!
//! A [`Journal`] is a cheap clone; [`Journal::disabled`] makes every call a
//! no-op (tests, CLI tools, a journal that failed to open).

pub mod cpu;
pub mod probe;
pub mod queue;
pub mod sanitize;
pub mod store;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::event_sink::EventSink;
use probe::RunProbe;
use queue::QueueMeter;
use store::{Limits, SessionWriter};

/// Bumped when the meaning of a field changes.
pub const SCHEMA: u32 = 1;

/// Pending records; a few seconds of the busiest observed rate.
const CHANNEL_CAPACITY: usize = 4096;
const FLUSH_EVERY: Duration = Duration::from_secs(2);
/// Resolution of the queue tracking around encode operations.
const TICK: Duration = Duration::from_millis(100);
const TICKS_PER_SAMPLE: u32 = 10;
/// An operation's impact is over once the queue is back this close to its
/// level before the operation.
const RECOVERED_MARGIN_MS: f64 = 100.0;
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(60);

enum Msg {
    Line(String),
    Flush(SyncSender<()>),
    Close(SyncSender<()>),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

/// Queue history around one encode operation.
struct OpTrack {
    id: u32,
    name: &'static str,
    run: Option<u32>,
    before_ms: f64,
    max_ms: f64,
    ended: Option<(Instant, f64)>,
}

struct Inner {
    started: Instant,
    tx: SyncSender<Msg>,
    dir: PathBuf,
    lost: AtomicU64,
    closed: AtomicBool,
    had_activity: AtomicBool,
    next_run: AtomicU32,
    next_op: AtomicU32,
    current: Mutex<Weak<RunProbe>>,
    ops: Mutex<Vec<OpTrack>>,
}

/// Handle on the application's journal. Cheap to clone.
#[derive(Clone, Default)]
pub struct Journal {
    inner: Option<Arc<Inner>>,
}

impl Journal {
    /// A journal that records nothing.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Start a session under `root`. `about` is merged into the header and
    /// must be publishable.
    pub fn open(root: &Path, about: Map<String, Value>, limits: Limits) -> io::Result<Self> {
        let mut header = about;
        header.insert("kind".into(), "header".into());
        header.insert("schema".into(), SCHEMA.into());
        header.insert(
            "host".into(),
            serde_json::to_value(cpu::HostInfo::detect()).unwrap_or(Value::Null),
        );
        header.insert(
            "utc".into(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .into(),
        );
        let writer = SessionWriter::create(root, &Value::Object(header), limits)?;
        let dir = writer.dir().to_path_buf();
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let inner = Arc::new(Inner::new(tx, dir));
        thread::Builder::new()
            .name("journal-writer".into())
            .spawn(move || run_writer(writer, rx))?;
        let weak = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("journal-probe".into())
            .spawn(move || run_sampler(weak))?;
        Ok(Self { inner: Some(inner) })
    }

    /// Directory of this session, `None` when disabled.
    pub fn dir(&self) -> Option<&Path> {
        self.inner.as_deref().map(|i| i.dir.as_path())
    }

    /// True once a worker or an encode operation has run.
    pub fn had_activity(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.had_activity.load(Ordering::Relaxed))
    }

    /// Append one record. Never blocks.
    pub fn record(&self, kind: &str, run: Option<u32>, mut fields: Map<String, Value>) {
        let Some(inner) = self.inner.as_ref() else { return };
        if inner.closed.load(Ordering::Relaxed) {
            return;
        }
        if !fields.contains_key("queue_ms") {
            if let Some(p) = inner.current_probe() {
                if run.map_or(true, |r| r == p.run()) {
                    fields.insert("queue_ms".into(), p.queue_ms().into());
                }
            }
        }
        fields.insert("t_ms".into(), (inner.started.elapsed().as_millis() as u64).into());
        fields.insert("kind".into(), kind.into());
        if let Some(run) = run {
            fields.insert("run".into(), run.into());
        }
        let line = Value::Object(fields).to_string();
        if inner.tx.try_send(Msg::Line(line)).is_err() {
            inner.lost.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Register a worker; keep the probe for as long as the worker runs.
    pub fn worker_started(&self, meter: Arc<QueueMeter>, about: Map<String, Value>) -> Arc<RunProbe> {
        let run = self
            .inner
            .as_ref()
            .map_or(0, |i| i.next_run.fetch_add(1, Ordering::Relaxed));
        let probe = RunProbe::new(run, self.clone(), meter);
        if let Some(inner) = self.inner.as_ref() {
            *lock(&inner.current) = Arc::downgrade(&probe);
            inner.had_activity.store(true, Ordering::Relaxed);
            self.record("run_start", Some(run), about);
        }
        probe
    }

    /// The worker left its loop: whatever is still queued will not be decoded.
    pub fn worker_stopped(&self, probe: &Arc<RunProbe>) {
        let Some(inner) = self.inner.as_ref() else { return };
        let mut f = probe.sample();
        f.insert("abandoned_ms".into(), probe.queue_ms().into());
        self.record("run_stop", Some(probe.run()), f);
        let mut current = lock(&inner.current);
        // A restart may already have registered the next worker.
        if std::ptr::eq(current.as_ptr(), Arc::as_ptr(probe)) {
            *current = Weak::new();
        }
    }

    /// Track an encode operation and its effect on the RX queue.
    pub fn start_op(&self, name: &'static str, mut fields: Map<String, Value>) -> Op {
        let mut id = 0;
        if let Some(inner) = self.inner.as_ref() {
            inner.had_activity.store(true, Ordering::Relaxed);
            id = inner.next_op.fetch_add(1, Ordering::Relaxed);
            let current = inner.current_probe();
            let before_ms = current.as_ref().map_or(0.0, |p| p.queue_ms());
            lock(&inner.ops).push(OpTrack {
                id,
                name,
                run: current.as_ref().map(|p| p.run()),
                before_ms,
                max_ms: before_ms,
                ended: None,
            });
            fields.insert("op".into(), id.into());
            fields.insert("name".into(), name.into());
            self.record("op_start", None, fields);
        }
        Op {
            journal: self.clone(),
            id,
            name,
            started: Instant::now(),
            process_cpu: cpu::process_cpu(),
            stage_started: Instant::now(),
            stage_cpu: cpu::thread_cpu(),
            result: Map::new(),
            ok: false,
        }
    }

    /// Wait up to `timeout` for pending records to reach the OS. Usable
    /// from the panic hook.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.sync(timeout, Msg::Flush)
    }

    /// Orderly end of session: write the closing record and marker.
    pub fn close(&self, timeout: Duration) -> bool {
        let Some(inner) = self.inner.as_ref() else { return true };
        let mut f = Map::new();
        f.insert("lost_records".into(), inner.lost.load(Ordering::Relaxed).into());
        self.record("close", None, f);
        if inner.closed.swap(true, Ordering::Relaxed) {
            return true;
        }
        self.sync(timeout, Msg::Close)
    }

    fn sync(&self, timeout: Duration, msg: fn(SyncSender<()>) -> Msg) -> bool {
        let Some(inner) = self.inner.as_ref() else { return true };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + timeout;
        let mut pending = msg(ack_tx);
        // `send` would block forever behind a wedged disk.
        loop {
            match inner.tx.try_send(pending) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
                Err(mpsc::TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    pending = back;
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
        ack_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
    }
}

impl Inner {
    fn new(tx: SyncSender<Msg>, dir: PathBuf) -> Self {
        Self {
            started: Instant::now(),
            tx,
            dir,
            lost: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            had_activity: AtomicBool::new(false),
            next_run: AtomicU32::new(1),
            next_op: AtomicU32::new(1),
            current: Mutex::new(Weak::new()),
            ops: Mutex::new(Vec::new()),
        }
    }

    fn current_probe(&self) -> Option<Arc<RunProbe>> {
        lock(&self.current).upgrade()
    }
}

fn run_writer(writer: SessionWriter, rx: mpsc::Receiver<Msg>) {
    let mut writer = Some(writer);
    let mut last_flush = Instant::now();
    loop {
        match rx.recv_timeout(FLUSH_EVERY) {
            Ok(Msg::Line(line)) => {
                if let Some(w) = writer.as_mut() {
                    if let Err(e) = w.write_line(&line) {
                        eprintln!("[journal] write failed, journal stopped: {e}");
                        writer = None;
                    }
                }
            }
            Ok(Msg::Flush(ack)) => {
                if let Some(w) = writer.as_mut() {
                    let _ = w.flush();
                }
                let _ = ack.try_send(());
            }
            Ok(Msg::Close(ack)) => {
                if let Some(w) = writer.take() {
                    if let Err(e) = w.close() {
                        eprintln!("[journal] close failed: {e}");
                    }
                }
                let _ = ack.try_send(());
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            if let Some(w) = writer.as_mut() {
                let _ = w.flush();
            }
            last_flush = Instant::now();
        }
    }
    if let Some(w) = writer.as_mut() {
        let _ = w.flush();
    }
}

fn run_sampler(weak: Weak<Inner>) {
    let mut tick = 0u32;
    let mut last = (Instant::now(), cpu::process_cpu());
    loop {
        thread::sleep(TICK);
        let Some(inner) = weak.upgrade() else { return };
        if inner.closed.load(Ordering::Relaxed) {
            return;
        }
        let journal = Journal {
            inner: Some(inner.clone()),
        };
        track_ops(&journal, &inner);

        tick += 1;
        if tick < TICKS_PER_SAMPLE {
            continue;
        }
        tick = 0;

        let probe = inner.current_probe();
        let mut f = probe.as_ref().map(|p| p.sample()).unwrap_or_default();
        let now = (Instant::now(), cpu::process_cpu());
        if let Some((before, after)) = last.1.zip(now.1) {
            let wall = now.0.duration_since(last.0).as_secs_f64();
            if wall > 0.0 {
                // Cores' worth of CPU used by the whole process.
                let cores = after.saturating_sub(before).as_secs_f64() / wall;
                f.insert("proc_cores".into(), ((cores * 100.0).round() / 100.0).into());
            }
        }
        last = now;
        let running: Vec<&'static str> = lock(&inner.ops)
            .iter()
            .filter(|op| op.ended.is_none())
            .map(|op| op.name)
            .collect();
        if !running.is_empty() {
            f.insert("ops".into(), running.into());
            if let Some(rss) = cpu::rss_mib() {
                f.insert("rss_mib".into(), rss.into());
            }
        }
        let lost = inner.lost.load(Ordering::Relaxed);
        if lost > 0 {
            f.insert("lost_records".into(), lost.into());
        }
        // `buffer` when a worker runs, `idle` otherwise.
        let kind = if probe.is_some() { "buffer" } else { "idle" };
        journal.record(kind, probe.map(|p| p.run()), f);
    }
}

/// Follow the queue during each operation and until it has recovered.
fn track_ops(journal: &Journal, inner: &Inner) {
    let current = inner.current_probe();
    let mut finished = Vec::new();
    {
        let mut ops = lock(&inner.ops);
        ops.retain_mut(|op| {
            let queue = current
                .as_ref()
                .filter(|p| Some(p.run()) == op.run)
                .map(|p| p.queue_ms());
            if let Some(q) = queue {
                op.max_ms = op.max_ms.max(q);
            }
            let Some((ended, at_end_ms)) = op.ended else { return true };
            let outcome = match queue {
                None if op.run.is_none() => "no_rx",
                None => "rx_stopped",
                Some(q) if q <= op.before_ms + RECOVERED_MARGIN_MS => "recovered",
                Some(_) if ended.elapsed() >= RECOVERY_TIMEOUT => "not_recovered",
                Some(_) => return true,
            };
            let mut f = Map::new();
            f.insert("op".into(), op.id.into());
            f.insert("name".into(), op.name.into());
            f.insert("outcome".into(), outcome.into());
            if op.run.is_some() {
                f.insert("queue_before_ms".into(), op.before_ms.into());
                f.insert("queue_peak_ms".into(), op.max_ms.into());
                f.insert("queue_end_ms".into(), at_end_ms.into());
                f.insert("recovery_ms".into(), ms(ended.elapsed()).into());
            }
            finished.push((op.run, f));
            false
        });
    }
    for (run, f) in finished {
        journal.record("op_impact", run, f);
    }
}

/// An encode operation in progress. Dropping it without [`Op::succeed`]
/// records a failure.
pub struct Op {
    journal: Journal,
    id: u32,
    name: &'static str,
    started: Instant,
    process_cpu: Option<Duration>,
    stage_started: Instant,
    stage_cpu: Option<Duration>,
    result: Map<String, Value>,
    ok: bool,
}

impl Op {
    /// Record the stage that just finished.
    pub fn stage(&mut self, stage: &'static str) {
        let thread_cpu = cpu::thread_cpu();
        let mut f = Map::new();
        f.insert("op".into(), self.id.into());
        f.insert("stage".into(), stage.into());
        f.insert("ms".into(), ms(self.stage_started.elapsed()).into());
        if let Some((before, after)) = self.stage_cpu.zip(thread_cpu) {
            f.insert("cpu_ms".into(), ms(after.saturating_sub(before)).into());
        }
        if let Some(rss) = cpu::rss_mib() {
            f.insert("rss_mib".into(), rss.into());
        }
        self.journal.record("op_stage", None, f);
        self.stage_started = Instant::now();
        self.stage_cpu = thread_cpu;
    }

    /// Attach a publishable result value (sizes, counts).
    pub fn set(&mut self, key: &'static str, value: impl Into<Value>) {
        self.result.insert(key.into(), value.into());
    }

    pub fn succeed(mut self) {
        self.ok = true;
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        let Some(inner) = self.journal.inner.as_ref() else { return };
        let mut f = std::mem::take(&mut self.result);
        f.insert("op".into(), self.id.into());
        f.insert("name".into(), self.name.into());
        f.insert("ok".into(), self.ok.into());
        f.insert("ms".into(), ms(self.started.elapsed()).into());
        if let Some((before, after)) = self.process_cpu.zip(cpu::process_cpu()) {
            // All threads: the encoder's pool, but the RX worker too.
            f.insert("proc_cpu_ms".into(), ms(after.saturating_sub(before)).into());
        }
        let queue = inner.current_probe().map(|p| (p.run(), p.queue_ms()));
        if let Some(op) = lock(&inner.ops).iter_mut().find(|op| op.id == self.id) {
            let at_end = queue.filter(|q| Some(q.0) == op.run).map_or(0.0, |q| q.1);
            op.max_ms = op.max_ms.max(at_end);
            op.ended = Some((Instant::now(), at_end));
        }
        self.journal.record("op_end", None, f);
    }
}

/// Copies the publishable part of a worker's events into the journal, then
/// forwards them unchanged.
pub struct JournalSink {
    inner: Arc<dyn EventSink>,
    journal: Journal,
    run: Option<u32>,
}

impl JournalSink {
    pub fn new(inner: Arc<dyn EventSink>, journal: Journal, run: Option<u32>) -> Self {
        Self { inner, journal, run }
    }
}

impl EventSink for JournalSink {
    fn emit_json(&self, name: &str, payload: Value) {
        if self.journal.inner.is_some() {
            if let Some((kind, fields)) = sanitize::event_record(name, &payload) {
                self.journal.record(&kind, self.run, fields);
            }
        }
        self.inner.emit_json(name, payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_sink::RecordingSink;
    use queue::MeteredReceiver;
    use serde_json::json;

    fn read_session(dir: &Path) -> Vec<Value> {
        use std::io::Read;
        let gz = store::bundle(dir, 1 << 24).unwrap().gz;
        let mut text = String::new();
        flate2::read::MultiGzDecoder::new(&gz[..])
            .read_to_string(&mut text)
            .unwrap();
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn of_kind<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
        records.iter().filter(|r| r["kind"] == kind).collect()
    }

    #[test]
    fn blocked_writer_never_blocks_the_caller() {
        // Nobody drains the channel: a wedged disk.
        let (tx, _rx) = mpsc::sync_channel(8);
        let inner = Arc::new(Inner::new(tx, PathBuf::new()));
        let journal = Journal { inner: Some(inner.clone()) };
        let t0 = Instant::now();
        for _ in 0..10_000 {
            journal.record("x", None, Map::new());
        }
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(inner.lost.load(Ordering::Relaxed), 10_000 - 8);
        assert!(!journal.flush(Duration::from_millis(50)));
    }

    #[test]
    fn disabled_journal_is_inert() {
        let journal = Journal::disabled();
        journal.record("x", None, Map::new());
        journal.start_op("avif", Map::new()).succeed();
        assert!(!journal.had_activity());
        assert!(journal.close(Duration::from_millis(1)));
    }

    #[test]
    fn restart_reports_abandoned_audio() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path(), Map::new(), Limits::default()).unwrap();
        let (tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        let first = journal.worker_started(metered.meter(), Map::new());
        tx.send(vec![0.0f32; 48_000]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while first.queue_ms() < 1000.0 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        journal.worker_stopped(&first);

        let (_tx2, rx2) = mpsc::channel();
        let second_rx = MeteredReceiver::new(rx2);
        let second = journal.worker_started(second_rx.meter(), Map::new());
        assert_ne!(first.run(), second.run());
        assert_eq!(second.queue_ms(), 0.0);

        assert!(journal.close(Duration::from_secs(5)));
        let records = read_session(journal.dir().unwrap());
        assert_eq!(of_kind(&records, "run_stop")[0]["abandoned_ms"], 1000.0);
        assert_eq!(of_kind(&records, "run_start").len(), 2);
        assert!(store::unfinished(root.path()).is_empty());
    }

    #[test]
    fn operation_reports_its_impact_on_the_queue() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path(), Map::new(), Limits::default()).unwrap();
        let (tx, rx) = mpsc::channel();
        let metered = MeteredReceiver::new(rx);
        let _probe = journal.worker_started(metered.meter(), Map::new());

        let mut op = journal.start_op("avif", Map::new());
        tx.send(vec![0.0f32; 24_000]).unwrap();
        thread::sleep(TICK * 4);
        op.stage("encode");
        op.succeed();
        while metered.try_recv().is_ok() {}
        thread::sleep(TICK * 4);

        assert!(journal.close(Duration::from_secs(5)));
        let records = read_session(journal.dir().unwrap());
        let impact = of_kind(&records, "op_impact")[0];
        assert_eq!(impact["queue_before_ms"], 0.0);
        assert_eq!(impact["queue_peak_ms"], 500.0);
        assert_eq!(impact["outcome"], "recovered");
        assert_eq!(of_kind(&records, "op_stage")[0]["stage"], "encode");
    }

    #[test]
    fn sink_forwards_everything_and_logs_the_publishable_part() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path(), Map::new(), Limits::default()).unwrap();
        let gui = Arc::new(RecordingSink::new());
        let sink = JournalSink::new(gui.clone(), journal.clone(), Some(1));
        sink.emit_json("envelope", json!({"filename": "a.avif", "callsign": "hb9tob/p"}));
        assert_eq!(gui.events()[0].1["filename"], "a.avif");

        assert!(journal.close(Duration::from_secs(5)));
        let records = read_session(journal.dir().unwrap());
        let logged = of_kind(&records, "envelope")[0];
        assert_eq!(logged["callsign"], "HB9TOB/P");
        assert!(logged.get("filename").is_none());
    }
}
