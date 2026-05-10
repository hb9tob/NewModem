//! cpal capture thread → 48 kHz mono f32 samples channel.
//!
//! The cpal `Stream` is not `Send` on Windows (WASAPI/COM is thread-bound),
//! so we own it inside a dedicated capture thread that just parks on a stop
//! flag. Samples are forwarded to a **bounded** `sync_channel` consumed by
//! the rx worker.
//!
//! ## Backpressure & chunk shape (mirrors `modem_pluto::rx`, `modem_sdrplay::rx`)
//!
//! cpal calls the audio callback at a device-dependent cadence (typically
//! ~25 ms on PulseAudio @ 48 kHz, but as small as 5 ms on a Pi4 with a
//! tight ALSA buffer). Each callback delivers a small `&[T]` of interleaved
//! samples. We do two things to keep the pipeline aligned with the SDR
//! backends:
//!
//! 1. **Per-stream `pending` accumulator** — every callback converts to
//!    mono f32 and appends to a `Vec<f32>` captured in the closure (the
//!    closure is `FnMut`, so cpal carries this state across calls). When
//!    `pending` reaches [`TARGET_CHUNK_SAMPLES`] (1200 = 25 ms) we flush
//!    one chunk to the worker. This matches what Pluto and SDRplay send,
//!    so the worker side stays homogeneous and we don't drown a small
//!    bounded channel with N tiny sends per cpal callback.
//!
//! 2. **Bounded `sync_channel`** sized at [`CHANNEL_CAPACITY_CHUNKS`]
//!    chunks (~800 ms cushion). On a healthy machine the worker drains
//!    far faster than we produce, so the channel stays near-empty.
//!    When the worker stalls (e.g. a 200-300 ms `rx_v3` tick on a Pi5)
//!    the cushion absorbs the spike. When the consumer is *chronically*
//!    too slow (Pi4 / very old PCs), the channel fills, [`try_send`]
//!    returns `Full`, and the dropped sample count is incremented in
//!    [`CaptureHandle::dropped_samples`]. Dropping is the correct
//!    behaviour here — the alternative (`send` on a bounded channel) is
//!    blocking inside the OS audio thread, which causes the sound card
//!    to underrun. Better to lose samples cleanly with a visible
//!    counter than to cascade-fail invisibly.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, BuildStreamError, SampleFormat, SampleRate, SupportedBufferSize};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TARGET_RATE: u32 = 48_000;

/// Audio samples per `Vec<f32>` flushed to the worker mpsc. 1200 samples
/// = 25 ms at 48 kHz. Same constant as `modem_pluto::rx::TARGET_CHUNK_SAMPLES`
/// and `modem_sdrplay::rx::TARGET_CHUNK_SAMPLES`. Keeps the worker side
/// homogeneous across capture backends.
const TARGET_CHUNK_SAMPLES: usize = 1200;

/// Bounded sync_channel capacity (in chunks). 32 × 25 ms = 800 ms cushion.
/// Big enough to absorb the worst observed `rx_v3` tick on a Pi5
/// (~250 ms with HIGH56), small enough that a chronically overloaded CPU
/// (Pi4 with HIGH+) drops samples and surfaces a counter instead of
/// silently growing the heap. 32 × 1200 × 4 bytes ≈ 150 KiB of RAM,
/// trivial on every supported target.
const CHANNEL_CAPACITY_CHUNKS: usize = 32;

/// Target ALSA / WASAPI buffer in milliseconds. By default cpal lets the
/// backend choose, which on Linux/ALSA gives 5–20 ms — a window narrow
/// enough that any rx_worker stall longer than ~10 ms can cause a kernel
/// xrun (silent sample loss inside ALSA, surfacing as
/// `StreamError::InputOverflow` in our error counter without showing up
/// in `dropped_samples`). 100 ms relaxes the audio-thread scheduling
/// deadline drastically — the OS only needs to service the callback
/// every ~100 ms instead of every 5 ms — at the cost of 100 ms of
/// added input latency, which is invisible here because the modem
/// already buffers ≈ 800 ms in the bounded mpsc plus minutes in
/// `session_buffer`. Honoured on ALSA direct + CoreAudio + WASAPI
/// exclusive; **silently ignored** by PulseAudio / PipeWire / WASAPI
/// shared (they impose their own period). The fallback path triggers
/// if the device flat-out refuses the value.
const TARGET_BUFFER_MS: u32 = 100;

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("nbfm-capture.log")
}

fn log(msg: &str) {
    eprintln!("{msg}");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{msg}");
    }
}

pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Total mono samples that the bounded mpsc dropped because the
    /// worker couldn't keep up. Read by the rx_worker telemetry tick to
    /// surface a `rx_realtime` warning to the GUI; also logged in the
    /// 2 s capture tick so the failure mode is visible from the file
    /// log even without the GUI.
    pub dropped_samples: Arc<AtomicU64>,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

pub fn start(device_name: &str) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
    let (sample_tx, sample_rx) = mpsc::sync_channel::<Vec<f32>>(CHANNEL_CAPACITY_CHUNKS);
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u16), String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let dropped_samples = Arc::new(AtomicU64::new(0));
    let stop_thread = stop.clone();
    let dropped_thread = dropped_samples.clone();
    let device_name = device_name.to_string();

    let thread = thread::spawn(move || {
        run_capture(&device_name, sample_tx, ready_tx, stop_thread, dropped_thread);
    });

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok((sample_rate, channels))) => Ok((
            CaptureHandle {
                stop,
                thread: Some(thread),
                sample_rate,
                channels,
                dropped_samples,
            },
            sample_rx,
        )),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("timeout waiting for capture thread to start".into()),
    }
}

fn run_capture(
    device_name: &str,
    sample_tx: SyncSender<Vec<f32>>,
    ready_tx: mpsc::Sender<Result<(u32, u16), String>>,
    stop: Arc<AtomicBool>,
    dropped_samples: Arc<AtomicU64>,
) {
    let host = cpal::default_host();
    let device = match host.input_devices() {
        Ok(mut iter) => iter.find(|d| d.name().map(|n| n == device_name).unwrap_or(false)),
        Err(e) => {
            let _ = ready_tx.send(Err(format!("input_devices: {e}")));
            return;
        }
    };
    let Some(device) = device else {
        let _ = ready_tx.send(Err(format!("device '{device_name}' not found")));
        return;
    };

    let configs = match device.supported_input_configs() {
        Ok(c) => c.collect::<Vec<_>>(),
        Err(e) => {
            let _ = ready_tx.send(Err(format!("supported_input_configs: {e}")));
            return;
        }
    };
    // Keep only configs that cover 48 kHz, then pick by format preference :
    // F32 > I16 > U16 > U8 (mirror of what we can decode). This avoids
    // picking an unsupported format first when the device also advertises
    // a usable one.
    let supports_48k: Vec<_> = configs
        .into_iter()
        .filter(|c| c.min_sample_rate().0 <= TARGET_RATE && TARGET_RATE <= c.max_sample_rate().0)
        .collect();
    if supports_48k.is_empty() {
        let _ = ready_tx.send(Err(format!(
            "device '{device_name}' does not support {TARGET_RATE} Hz"
        )));
        return;
    }
    fn rank(f: SampleFormat) -> u8 {
        match f {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            SampleFormat::U16 => 2,
            SampleFormat::U8 => 3,
            _ => 4,
        }
    }
    let range = supports_48k
        .into_iter()
        .min_by_key(|c| rank(c.sample_format()))
        .unwrap();
    // Capture the supported buffer-size range BEFORE `with_sample_rate`
    // takes ownership of `range`. `SupportedBufferSize` is `Copy`.
    let supported_buf = *range.buffer_size();
    let format = range.sample_format();
    let cfg = range.with_sample_rate(SampleRate(TARGET_RATE));
    let channels = cfg.channels();
    let mut stream_cfg: cpal::StreamConfig = cfg.into();

    // Pre-validate the format so the build closure below doesn't have to
    // carry an "unsupported" branch (and we keep the existing error
    // wording for the user-facing message).
    if !matches!(
        format,
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16 | SampleFormat::U8
    ) {
        let _ = ready_tx.send(Err(format!("unsupported sample format: {format:?}")));
        return;
    }

    // Pick a buffer size in line with `TARGET_BUFFER_MS`, clamped to the
    // device-reported range. If the range is `Unknown` we fall through
    // to `Default` — same behaviour as the historical code path.
    let target_frames = TARGET_RATE * TARGET_BUFFER_MS / 1000;
    let chosen_buf = match supported_buf {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed(target_frames.clamp(min, max))
        }
        SupportedBufferSize::Unknown => BufferSize::Default,
    };

    let _ = std::fs::remove_file(log_path());
    log(&format!(
        "[capture] building stream : device='{device_name}' format={format:?} channels={channels} target={TARGET_RATE}Hz supported_buf={supported_buf:?} requested_buf={chosen_buf:?}"
    ));

    let callback_count = Arc::new(AtomicU64::new(0));
    let callback_samples = Arc::new(AtomicU64::new(0));
    // Errors are throttled because cpal's ALSA backend can refire POLLERR
    // millions of times per second (observed: 1.06 M lines, ~16 GiB log
    // file in a few hours when the audio device misbehaved). We still
    // surface the count in the periodic tick line so the user sees the
    // stream is unhealthy.
    let error_count = Arc::new(AtomicU64::new(0));

    // The build closure clones every shared handle on each invocation so
    // the retry path (Fixed → Default fallback) starts from a clean slate.
    // It's `FnMut` only because `pending` mutates inside the per-format
    // audio callback; the closure itself only borrows `device` /
    // `dropped_samples` / `sample_tx` / atomics by reference, and produces
    // a fresh `cpal::Stream` each call.
    let try_build = |stream_cfg: &cpal::StreamConfig|
        -> Result<cpal::Stream, BuildStreamError> {
        let dropped_cb = dropped_samples.clone();
        let tx = sample_tx.clone();
        let cb_count = callback_count.clone();
        let cb_samples = callback_samples.clone();
        let err_count = error_count.clone();
        match format {
            SampleFormat::F32 => {
                let mut pending: Vec<f32> = Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2);
                device.build_input_stream::<f32, _, _>(
                    stream_cfg,
                    move |data, _| {
                        cb_count.fetch_add(1, Ordering::Relaxed);
                        cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                        pending.extend(mono_iter_f32(data, channels));
                        flush_pending(&mut pending, &tx, &dropped_cb);
                    },
                    make_err_cb(err_count),
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut pending: Vec<f32> = Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2);
                device.build_input_stream::<i16, _, _>(
                    stream_cfg,
                    move |data, _| {
                        cb_count.fetch_add(1, Ordering::Relaxed);
                        cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                        pending.extend(mono_iter_i16(data, channels));
                        flush_pending(&mut pending, &tx, &dropped_cb);
                    },
                    make_err_cb(err_count),
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut pending: Vec<f32> = Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2);
                device.build_input_stream::<u16, _, _>(
                    stream_cfg,
                    move |data, _| {
                        cb_count.fetch_add(1, Ordering::Relaxed);
                        cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                        pending.extend(mono_iter_u16(data, channels));
                        flush_pending(&mut pending, &tx, &dropped_cb);
                    },
                    make_err_cb(err_count),
                    None,
                )
            }
            SampleFormat::U8 => {
                let mut pending: Vec<f32> = Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2);
                device.build_input_stream::<u8, _, _>(
                    stream_cfg,
                    move |data, _| {
                        cb_count.fetch_add(1, Ordering::Relaxed);
                        cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                        pending.extend(mono_iter_u8(data, channels));
                        flush_pending(&mut pending, &tx, &dropped_cb);
                    },
                    make_err_cb(err_count),
                    None,
                )
            }
            // Unreachable: filtered out above so the closure stays total.
            _ => unreachable!("format pre-validated above"),
        }
    };

    // First attempt: the configured `Fixed(N)` (or `Default` if the
    // device didn't expose a range). Keep `effective_buf` in sync with
    // what we end up actually using so the startup log + tick log can
    // report the truth.
    stream_cfg.buffer_size = chosen_buf;
    let mut effective_buf = chosen_buf;
    let stream = match try_build(&stream_cfg) {
        Ok(s) => s,
        // Fixed refused (e.g. driver advertises a range that's not
        // honoured by the underlying ALSA params, PipeWire shim hiccup,
        // …). Fall back to `Default`: never breaks an existing config,
        // just reverts to the historical small-buffer behaviour and
        // leaves a clear breadcrumb in the capture log.
        Err(e) if matches!(chosen_buf, BufferSize::Fixed(_)) => {
            log(&format!(
                "[capture] {chosen_buf:?} refused ({e}), falling back to BufferSize::Default"
            ));
            stream_cfg.buffer_size = BufferSize::Default;
            effective_buf = BufferSize::Default;
            match try_build(&stream_cfg) {
                Ok(s) => s,
                Err(e2) => {
                    let _ = ready_tx.send(Err(format!(
                        "build_input_stream (Default fallback): {e2}"
                    )));
                    return;
                }
            }
        }
        Err(e) => {
            let _ = ready_tx.send(Err(format!("build_input_stream: {e}")));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(format!("stream.play: {e}")));
        return;
    }

    // Convert `effective_buf` to a duration when known. With `Default`
    // ALSA negotiates its own size that we can only observe from the
    // callback period — see the tick loop below.
    let buf_label = match effective_buf {
        BufferSize::Fixed(n) => {
            format!("Fixed({n}) ≈ {:.0} ms", n as f64 * 1000.0 / TARGET_RATE as f64)
        }
        BufferSize::Default => "Default (taille négociée par le backend, voir tick)".to_string(),
    };
    log(&format!(
        "[capture] stream.play OK — device='{device_name}' rate={TARGET_RATE} channels={channels} format={format:?} buffer={buf_label} chunk={TARGET_CHUNK_SAMPLES} channel_cap={CHANNEL_CAPACITY_CHUNKS}"
    ));
    let _ = ready_tx.send(Ok((TARGET_RATE, channels)));

    let mut last_tick = Instant::now();
    let mut last_samples: u64 = 0;
    let mut last_count: u64 = 0;
    let mut last_errors: u64 = 0;
    let mut last_dropped: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        let now = Instant::now();
        if now.duration_since(last_tick) >= Duration::from_secs(2) {
            let cnt = callback_count.load(Ordering::Relaxed);
            let smp = callback_samples.load(Ordering::Relaxed);
            let err = error_count.load(Ordering::Relaxed);
            let drp = dropped_samples.load(Ordering::Relaxed);
            let dt = now.duration_since(last_tick).as_secs_f64();
            let cb_delta = cnt - last_count;
            let smp_delta = smp - last_samples;
            let cb_per_sec = cb_delta as f64 / dt;
            let smp_per_sec = smp_delta as f64 / dt;
            // Observed callback period: `data.len()` is interleaved
            // samples, so frames-per-callback = samples / channels, and
            // period = frames / sample_rate. With `BufferSize::Default`
            // this is the only way to know what the backend negotiated.
            let period_ms = if cb_delta > 0 {
                let frames_per_cb = (smp_delta / channels as u64) as f64 / cb_delta as f64;
                frames_per_cb * 1000.0 / TARGET_RATE as f64
            } else {
                0.0
            };
            log(&format!(
                "[capture] tick: cbs={cnt} (+{cb_delta} in {dt:.1}s, {cb_per_sec:.1}/s), samples={smp} ({smp_per_sec:.0} Sa/s ≈ {ratio:.1}× target), period={period_ms:.1}ms/cb, errors={err} (+{ed}), dropped={drp} (+{dd})",
                ratio = smp_per_sec / (TARGET_RATE as f64 * channels as f64),
                ed = err - last_errors,
                dd = drp - last_dropped,
            ));
            last_tick = now;
            last_count = cnt;
            last_samples = smp;
            last_errors = err;
            last_dropped = drp;
        }
    }
    log("[capture] stop flag → dropping stream");
    drop(stream);
}

/// Builds a throttled error callback for cpal `build_input_stream`.
///
/// cpal forwards every backend error verbatim, and on Linux/ALSA a
/// `POLLERR` condition can refire on every poll iteration — easily a
/// million times per second. The naive "log every error" approach
/// produced multi-GiB logs in production, so we count errors via the
/// shared atomic and only emit a line on power-of-ten milestones (and
/// once per million afterwards). The current count is also exposed in
/// the periodic tick log, so a chronically failing stream stays
/// visible without flooding the disk.
fn make_err_cb(count: Arc<AtomicU64>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| {
        let n = count.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 3
            || n == 10
            || n == 100
            || n == 1_000
            || n == 10_000
            || n == 100_000
            || n % 1_000_000 == 0
        {
            log(&format!("[capture] stream error #{n}: {e}"));
        }
    }
}

/// Drain `pending` in fixed [`TARGET_CHUNK_SAMPLES`] chunks. Uses
/// `try_send` so a slow consumer never blocks the OS audio thread —
/// `Full` returns drop the chunk and increment `dropped`; `Disconnected`
/// is silent (worker has been torn down, the next tick will exit the
/// capture loop on the stop flag).
fn flush_pending(
    pending: &mut Vec<f32>,
    tx: &SyncSender<Vec<f32>>,
    dropped: &Arc<AtomicU64>,
) {
    while pending.len() >= TARGET_CHUNK_SAMPLES {
        let chunk: Vec<f32> = pending.drain(..TARGET_CHUNK_SAMPLES).collect();
        match tx.try_send(chunk) {
            Ok(()) => {}
            Err(TrySendError::Full(c)) => {
                dropped.fetch_add(c.len() as u64, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                // Worker is gone — nothing to do, the capture loop will
                // exit on the stop flag at the next tick.
                return;
            }
        }
    }
}

// ─────────────────────────────────────────────────────────── format conversion
//
// Each helper returns an iterator of f32 mono samples so the closure can
// `extend` its `pending` buffer in one call without an intermediate Vec.

fn mono_iter_f32<'a>(data: &'a [f32], channels: u16) -> Box<dyn Iterator<Item = f32> + 'a> {
    let ch = channels as usize;
    if ch <= 1 {
        Box::new(data.iter().copied())
    } else {
        Box::new(data.chunks_exact(ch).map(|c| c[0]))
    }
}

fn mono_iter_i16<'a>(data: &'a [i16], channels: u16) -> Box<dyn Iterator<Item = f32> + 'a> {
    let ch = channels as usize;
    const SCALE: f32 = 1.0 / 32768.0;
    if ch <= 1 {
        Box::new(data.iter().map(|&s| s as f32 * SCALE))
    } else {
        Box::new(data.chunks_exact(ch).map(|c| c[0] as f32 * SCALE))
    }
}

fn mono_iter_u16<'a>(data: &'a [u16], channels: u16) -> Box<dyn Iterator<Item = f32> + 'a> {
    let ch = channels as usize;
    const OFFSET: f32 = 32768.0;
    const SCALE: f32 = 1.0 / 32768.0;
    if ch <= 1 {
        Box::new(data.iter().map(|&s| (s as f32 - OFFSET) * SCALE))
    } else {
        Box::new(
            data.chunks_exact(ch)
                .map(|c| (c[0] as f32 - OFFSET) * SCALE),
        )
    }
}

fn mono_iter_u8<'a>(data: &'a [u8], channels: u16) -> Box<dyn Iterator<Item = f32> + 'a> {
    let ch = channels as usize;
    const OFFSET: f32 = 128.0;
    const SCALE: f32 = 1.0 / 128.0;
    if ch <= 1 {
        Box::new(data.iter().map(|&s| (s as f32 - OFFSET) * SCALE))
    } else {
        Box::new(
            data.chunks_exact(ch)
                .map(|c| (c[0] as f32 - OFFSET) * SCALE),
        )
    }
}
