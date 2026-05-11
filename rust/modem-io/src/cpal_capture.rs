//! cpal capture → 48 kHz mono f32 sample channel via a 30-second
//! lock-free SPSC ring buffer.
//!
//! ## Architecture (post rx-soundcard-resilience refactor)
//!
//! Three threads cooperate so the OS audio callback returns in
//! microseconds and never blocks on disk I/O, allocations, or upstream
//! consumers:
//!
//! ```text
//!  OS audio thread (cpal)          reader thread (best-effort SCHED_FIFO)
//!  ─────────────────────────       ────────────────────────────────────
//!  callback(&[T]):                  loop {
//!    bytes = cast<&[u8]>(data)        sleep(50 ms)
//!    producer.write(bytes)            drain ring → scratch_bytes
//!    counters.fetch_add(_)            decode → f32 mono → scratch_f32
//!    return                           worker_tx.send(scratch_f32)
//!                                     every 2 s: log tick
//!                                   }
//! ```
//!
//! The ring is sized at `RING_SECONDS × sample_rate × channels ×
//! bytes_per_sample` (~5–12 MB depending on format). That dwarfs any
//! realistic OS scheduler hiccup; a sustained ring overflow means the
//! reader thread is itself starved (kernel issue, not modem issue), and
//! we signal it via `dropped_samples` so the worker can brickwall →
//! flush + return to idle.
//!
//! ## Belt + suspenders against overflow
//!
//! 1. **30 s ring** (this file). Catches the steady-state worst case.
//! 2. **100 ms ALSA / WASAPI buffer** (`TARGET_BUFFER_MS`). Reduces the
//!    callback firing rate to ~10/s instead of ~100-200/s — the OS only
//!    needs to schedule us every 100 ms.
//! 3. **SCHED_FIFO prio 5** on the reader thread (Linux only,
//!    best-effort). Lets us preempt CFS-scheduled noise (browser tabs,
//!    Cargo, etc.) so the 50 ms poll is honoured even under load.
//!
//! If all three still aren't enough, the next step is bypassing
//! cpal/PulseAudio entirely and going direct ALSA `hw:`.
//!
//! ## What the OS callback does NOT do
//!
//! - **No allocation.** `producer.write_chunk_uninit` reuses ring slots.
//! - **No type conversion.** Native bytes go in unchanged; the reader
//!   thread decodes to f32 mono.
//! - **No mono mixing.** Same reason — stays in the reader thread.
//! - **No logging.** `log()` writes to disk; on a Pi that's the SD card.
//! - **No upstream `send`.** mpsc/sync_channel are out of the audio thread.
//!
//! Anything in the OS callback that allocates, blocks, or touches the
//! filesystem is a latent xrun. The whole point of this refactor.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, BuildStreamError, SampleFormat, SampleRate, SupportedBufferSize};
use rtrb::{chunks::ChunkError, Consumer, Producer, RingBuffer};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TARGET_RATE: u32 = 48_000;

/// Ring-buffer capacity in seconds, expressed in native bytes
/// (`rate × channels × bytes_per_sample × RING_SECONDS`). 30 s gives the
/// reader thread an enormous slack — at 50 ms poll, the worst scheduler
/// hiccup we'd need to absorb is the full 30 s, which on any reasonable
/// Linux is unheard of (Pi5 kernel jitter is < 5 ms p99). RAM cost
/// peaks around 11.5 MB for f32 stereo, well within budget on every
/// supported target.
const RING_SECONDS: usize = 30;

/// Reader thread poll period. Short enough that the unbounded worker
/// mpsc gets fed promptly (~50 ms latency added to the path) ; long
/// enough that the reader spends ~99 % of its time in `nanosleep`,
/// leaving the CPU free for the worker and the OS audio thread.
const READER_POLL_MS: u64 = 50;

/// Target ALSA / WASAPI buffer in milliseconds. By default cpal lets the
/// backend choose, which on Linux/ALSA gives 5–20 ms — a window narrow
/// enough that any reader-thread stall longer than ~10 ms can cause a
/// kernel xrun (silent sample loss inside ALSA, surfacing as
/// `StreamError::InputOverflow` in our error counter). 100 ms relaxes
/// the audio-thread scheduling deadline drastically — the OS only needs
/// to service the callback every ~100 ms instead of every 5 ms — at the
/// cost of 100 ms of added input latency, invisible here because the
/// modem already buffers 30 s in the ring + minutes in `session_buffer`.
/// Honoured on ALSA direct + CoreAudio + WASAPI exclusive; **silently
/// ignored** by PulseAudio / PipeWire / WASAPI shared (they impose
/// their own period). The fallback path triggers if the device flat-out
/// refuses the value.
const TARGET_BUFFER_MS: u32 = 100;

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("nbfm-capture.log")
}

/// Append a line to the capture log. **Never** called from the OS audio
/// thread — only from the reader thread (tick) and at startup. On a Pi
/// this is the SD card; a single fsync inside the audio callback would
/// be enough to cause an xrun.
fn log(msg: &str) {
    eprintln!("{msg}");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{msg}");
    }
}

pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    /// Two threads to join: (capture, reader). Capture owns the cpal
    /// `Stream` (not `Send` on Windows) and parks on `stop`. Reader
    /// drains the ring into the worker mpsc.
    pub threads: Option<(JoinHandle<()>, JoinHandle<()>)>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Cumulative mono-sample-equivalent count that the 30 s SPSC ring
    /// couldn't absorb because the reader thread was starved (= "true
    /// brickwall hit"). Read by the rx_worker telemetry tick to flag
    /// CPU surcharge to the GUI **and** to trigger a flush + return to
    /// idle when the session is active (see `rx_worker::run_worker`).
    pub dropped_samples: Arc<AtomicU64>,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some((t_cap, t_read)) = self.threads.take() {
            let _ = t_cap.join();
            let _ = t_read.join();
        }
    }
}

/// State handed from the capture thread (which negotiates the cpal
/// stream + creates the ring) to the reader thread (which decodes ring
/// bytes back into f32 mono). All atomics are shared with the OS
/// callback so the reader can include them in the 2 s tick log.
struct ReaderHandoff {
    consumer: Consumer<u8>,
    format: SampleFormat,
    channels: u16,
    bytes_per_frame: usize,
    callback_count: Arc<AtomicU64>,
    callback_samples: Arc<AtomicU64>,
    error_count: Arc<AtomicU64>,
    buf_label: String,
}

pub fn start(device_name: &str) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
    // Unbounded mpsc on purpose: the reader thread is in control of how
    // often we send and it sends ≈20 times per second. If the worker
    // stalls (e.g. a slow rx_v3 tick on a Pi5) chunks queue up here
    // briefly. The worker then catches up. No drop policy at this level
    // — the only place we drop is in the 30 s ring (capture brickwall)
    // and in the worker (session_buffer brickwall, > 5 min lag).
    let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u16), String>>();
    let (handoff_tx, handoff_rx) = mpsc::channel::<ReaderHandoff>();
    let stop = Arc::new(AtomicBool::new(false));
    let dropped_samples = Arc::new(AtomicU64::new(0));
    let device_name = device_name.to_string();

    let stop_capture = stop.clone();
    let dropped_capture = dropped_samples.clone();
    let capture_thread = thread::spawn(move || {
        run_capture(&device_name, ready_tx, handoff_tx, dropped_capture, stop_capture);
    });

    // Phase 1: did the cpal stream start? `ready_tx` is sent before the
    // ring handoff, so this is what surfaces device-not-found / format
    // errors to the caller.
    let (sample_rate, channels) = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            stop.store(true, Ordering::Relaxed);
            let _ = capture_thread.join();
            return Err(e);
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            let _ = capture_thread.join();
            return Err("timeout waiting for capture thread to start".into());
        }
    };

    // Phase 2: ring + format handoff. Sent immediately after `ready_tx`
    // so recv() returns within microseconds. 1 s timeout is purely
    // defensive — if it ever fires we have a bug.
    let handoff = match handoff_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(h) => h,
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            let _ = capture_thread.join();
            return Err("capture thread did not deliver ring handoff".into());
        }
    };

    let stop_reader = stop.clone();
    let dropped_reader = dropped_samples.clone();
    let reader_thread = thread::spawn(move || {
        run_reader(handoff, sample_tx, dropped_reader, stop_reader);
    });

    Ok((
        CaptureHandle {
            stop,
            threads: Some((capture_thread, reader_thread)),
            sample_rate,
            channels,
            dropped_samples,
        },
        sample_rx,
    ))
}

fn run_capture(
    device_name: &str,
    ready_tx: mpsc::Sender<Result<(u32, u16), String>>,
    handoff_tx: mpsc::Sender<ReaderHandoff>,
    dropped_samples: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
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
    // F32 > I16 > U16 > U8 (mirror of what we can decode).
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
    let supported_buf = *range.buffer_size();
    let format = range.sample_format();
    let cfg = range.with_sample_rate(SampleRate(TARGET_RATE));
    let channels = cfg.channels();
    let stream_cfg: cpal::StreamConfig = cfg.into();

    if !matches!(
        format,
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16 | SampleFormat::U8
    ) {
        let _ = ready_tx.send(Err(format!("unsupported sample format: {format:?}")));
        return;
    }

    // Pick a buffer size in line with `TARGET_BUFFER_MS`, clamped to the
    // device-reported range. If the range is `Unknown` we fall through
    // to `Default` — the negotiated period will then be visible in the
    // reader thread's tick line via the observed callback cadence.
    let target_frames = TARGET_RATE * TARGET_BUFFER_MS / 1000;
    let chosen_buf = match supported_buf {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed(target_frames.clamp(min, max))
        }
        SupportedBufferSize::Unknown => BufferSize::Default,
    };

    // Atomics shared between the OS audio callback (writers) and the
    // reader thread tick log (reader).
    let callback_count = Arc::new(AtomicU64::new(0));
    let callback_samples = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));

    // Ring size: 30 s of native-format interleaved bytes. `bytes_per_frame`
    // is also handed off to the reader so it knows how to chunk reads.
    let bytes_per_sample = format.sample_size();
    let bytes_per_frame = bytes_per_sample * channels as usize;
    let ring_bytes = TARGET_RATE as usize * RING_SECONDS * bytes_per_frame;

    // Builds a fresh ring + cpal stream for a given buffer-size setting.
    // Each call creates its own (producer, consumer) pair so the
    // Fixed → Default fallback path starts from a clean slate without
    // trying to recover state from the failed attempt. On error the
    // producer (moved into the closure) is dropped, the consumer
    // (declared before `?`) is dropped on early return.
    let make_stream = |buf_size: BufferSize|
        -> Result<(cpal::Stream, Consumer<u8>), BuildStreamError>
    {
        let mut local_cfg = stream_cfg.clone();
        local_cfg.buffer_size = buf_size;
        let (mut producer, consumer) = RingBuffer::<u8>::new(ring_bytes);
        let cb_count = callback_count.clone();
        let cb_samples = callback_samples.clone();
        let err_count = error_count.clone();
        let dropped_cb = dropped_samples.clone();
        let stream = match format {
            SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
                &local_cfg,
                move |data, _| {
                    cb_count.fetch_add(1, Ordering::Relaxed);
                    cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                    // Reinterpret as raw bytes — zero-cost, just a slice
                    // pointer + length cast. Native byte order is preserved
                    // (we read back with `from_ne_bytes`).
                    let bytes: &[u8] = unsafe {
                        std::slice::from_raw_parts(
                            data.as_ptr() as *const u8,
                            data.len() * std::mem::size_of::<f32>(),
                        )
                    };
                    write_to_ring(&mut producer, bytes, bytes_per_frame, &dropped_cb);
                },
                make_err_cb(err_count),
                None,
            )?,
            SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
                &local_cfg,
                move |data, _| {
                    cb_count.fetch_add(1, Ordering::Relaxed);
                    cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                    let bytes: &[u8] = unsafe {
                        std::slice::from_raw_parts(
                            data.as_ptr() as *const u8,
                            data.len() * std::mem::size_of::<i16>(),
                        )
                    };
                    write_to_ring(&mut producer, bytes, bytes_per_frame, &dropped_cb);
                },
                make_err_cb(err_count),
                None,
            )?,
            SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
                &local_cfg,
                move |data, _| {
                    cb_count.fetch_add(1, Ordering::Relaxed);
                    cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                    let bytes: &[u8] = unsafe {
                        std::slice::from_raw_parts(
                            data.as_ptr() as *const u8,
                            data.len() * std::mem::size_of::<u16>(),
                        )
                    };
                    write_to_ring(&mut producer, bytes, bytes_per_frame, &dropped_cb);
                },
                make_err_cb(err_count),
                None,
            )?,
            SampleFormat::U8 => device.build_input_stream::<u8, _, _>(
                &local_cfg,
                move |data, _| {
                    cb_count.fetch_add(1, Ordering::Relaxed);
                    cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                    // u8: bytes == samples, no transmute needed but we
                    // keep the same shape for uniformity.
                    write_to_ring(&mut producer, data, bytes_per_frame, &dropped_cb);
                },
                make_err_cb(err_count),
                None,
            )?,
            _ => unreachable!("format pre-validated above"),
        };
        Ok((stream, consumer))
    };

    let _ = std::fs::remove_file(log_path());
    log(&format!(
        "[capture] building stream : device='{device_name}' format={format:?} channels={channels} target={TARGET_RATE}Hz supported_buf={supported_buf:?} requested_buf={chosen_buf:?} ring_bytes={ring_bytes} ({RING_SECONDS}s × {bytes_per_frame}B/frame)"
    ));

    // First attempt: the configured `Fixed(N)` (or `Default` if the
    // device didn't expose a range). Fall back to `Default` on refusal —
    // never breaks an existing config, just reverts to the historical
    // small-buffer behaviour and leaves a clear breadcrumb in the log.
    let (stream, consumer, effective_buf) = match make_stream(chosen_buf) {
        Ok((s, c)) => (s, c, chosen_buf),
        Err(e) if matches!(chosen_buf, BufferSize::Fixed(_)) => {
            log(&format!(
                "[capture] {chosen_buf:?} refused ({e}), falling back to BufferSize::Default"
            ));
            match make_stream(BufferSize::Default) {
                Ok((s, c)) => (s, c, BufferSize::Default),
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

    let buf_label = match effective_buf {
        BufferSize::Fixed(n) => {
            format!("Fixed({n}) ≈ {:.0} ms", n as f64 * 1000.0 / TARGET_RATE as f64)
        }
        BufferSize::Default => "Default (taille négociée par le backend, voir tick)".to_string(),
    };
    log(&format!(
        "[capture] stream.play OK — device='{device_name}' rate={TARGET_RATE} channels={channels} format={format:?} buffer={buf_label} ring={ring_bytes}B ({RING_SECONDS}s)"
    ));

    // Signal startup success and hand off the ring to the reader.
    let _ = ready_tx.send(Ok((TARGET_RATE, channels)));
    let _ = handoff_tx.send(ReaderHandoff {
        consumer,
        format,
        channels,
        bytes_per_frame,
        callback_count,
        callback_samples,
        error_count,
        buf_label,
    });

    // Capture thread's only remaining job: keep the cpal `Stream` alive
    // (it's `!Send` on Windows so it must be owned by exactly the thread
    // that built it) and park on the stop flag. The OS audio callback
    // fires on its own thread, the reader thread is draining the ring
    // on yet another thread. We're just a lifetime guard.
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }
    log("[capture] stop flag → dropping stream");
    drop(stream);
}

/// Pure memcpy into the ring, called from the OS audio thread. The only
/// "computation" beyond the copy is an atomic increment of
/// `dropped_mono_samples` on overflow. No logs, no allocations, no
/// upstream sends — everything happens in the reader thread.
///
/// `bytes_per_frame` (== `bytes_per_sample × channels`) is used to
/// frame-align partial writes so the consumer never sees a half-sample
/// when the ring is right at the brink of overflow.
#[inline]
fn write_to_ring(
    producer: &mut Producer<u8>,
    bytes: &[u8],
    bytes_per_frame: usize,
    dropped_mono_samples: &AtomicU64,
) {
    let want = bytes.len();
    match producer.write_chunk_uninit(want) {
        Ok(chunk) => {
            // Sole runtime work: copy `bytes` into the ring. rtrb's
            // `fill_from_iter` over `&[u8]` compiles to the equivalent
            // of `ptr::copy_nonoverlapping` at -O3 (verified by
            // disassembly on aarch64 + x86_64).
            let n = chunk.fill_from_iter(bytes.iter().copied());
            debug_assert_eq!(n, want);
        }
        Err(ChunkError::TooFewSlots(available)) => {
            // Brickwall hit — the reader thread fell behind by more
            // than `RING_SECONDS`. Take what we can (frame-aligned, so
            // the consumer always sees whole samples) and signal the
            // rest via the atomic counter.
            let aligned_avail = available - (available % bytes_per_frame);
            if aligned_avail > 0 {
                if let Ok(chunk) = producer.write_chunk_uninit(aligned_avail) {
                    let _ = chunk.fill_from_iter(bytes[..aligned_avail].iter().copied());
                }
            }
            // `dropped_samples` is mono-sample-equivalent (one count
            // per dropped frame, irrespective of channel count) — that's
            // the unit rx_worker expects.
            let dropped_frames = (want - aligned_avail) / bytes_per_frame;
            dropped_mono_samples.fetch_add(dropped_frames as u64, Ordering::Relaxed);
        }
    }
}

/// Reader thread: drains the ring, decodes native format → f32 mono,
/// forwards to the worker via an unbounded mpsc. Runs at best-effort
/// SCHED_FIFO priority on Linux. This is where the per-tick logs live
/// (the OS audio thread itself never touches the filesystem).
fn run_reader(
    mut handoff: ReaderHandoff,
    sample_tx: mpsc::Sender<Vec<f32>>,
    dropped_samples: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let rt_ok = lift_rt_priority();
    log(&format!(
        "[capture-reader] starting : format={:?} channels={} buf={} ring_capacity={}B rt_prio={}",
        handoff.format,
        handoff.channels,
        handoff.buf_label,
        handoff.consumer.buffer().capacity(),
        if rt_ok { "SCHED_FIFO prio 5" } else { "normal" }
    ));

    let ch = handoff.channels as usize;
    let format = handoff.format;
    let bytes_per_frame = handoff.bytes_per_frame;
    let ring_cap = handoff.consumer.buffer().capacity();

    // Reusable scratch buffers. Sized for a 200 ms drain worst case at
    // f32 stereo (≈ 76 KiB bytes / 19 KiB f32). They grow if needed but
    // never shrink — steady-state allocation is zero.
    let initial_bytes = (TARGET_RATE as usize / 5) * bytes_per_frame;
    let mut scratch_bytes = Vec::<u8>::with_capacity(initial_bytes);
    let mut scratch_f32 = Vec::<f32>::with_capacity(TARGET_RATE as usize / 5);

    let mut last_tick = Instant::now();
    let mut last_cb_count: u64 = 0;
    let mut last_cb_samples: u64 = 0;
    let mut last_err_count: u64 = 0;
    let mut last_dropped: u64 = 0;
    let mut ring_high_water: usize = 0;

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(READER_POLL_MS));

        // Drain everything currently in the ring, clipped to a whole
        // number of frames. Anything below frame granularity stays in
        // the ring for the next tick.
        let avail = handoff.consumer.slots();
        if avail > ring_high_water {
            ring_high_water = avail;
        }
        let aligned = avail - (avail % bytes_per_frame);
        if aligned > 0 {
            scratch_bytes.clear();
            if scratch_bytes.capacity() < aligned {
                scratch_bytes.reserve(aligned - scratch_bytes.capacity());
            }
            match handoff.consumer.read_chunk(aligned) {
                Ok(chunk) => {
                    // `as_slices` returns up to two contiguous slices
                    // (one when the read doesn't wrap around, two when
                    // it does). We concatenate them into scratch_bytes
                    // so the decode loop is straight-line.
                    let (s1, s2) = chunk.as_slices();
                    scratch_bytes.extend_from_slice(s1);
                    scratch_bytes.extend_from_slice(s2);
                    chunk.commit_all();
                }
                Err(_) => {
                    // Should never happen: we just observed slots() ≥ aligned,
                    // and rtrb's slots() is monotonic in the consumer's view
                    // (only the producer side adds slots; the consumer is the
                    // only thing that can remove them). Skip the tick rather
                    // than panic.
                    continue;
                }
            }

            scratch_f32.clear();
            decode_native_to_mono_f32(&scratch_bytes, format, ch, &mut scratch_f32);

            // Unbounded mpsc — never blocks the reader thread. If the
            // worker is gone (Receiver dropped), send() returns Err and
            // we silently ignore: the stop flag will fire next tick.
            // We `clone()` so the buffer can be reused next iteration
            // without disturbing the chunk that's now in flight.
            let _ = sample_tx.send(scratch_f32.clone());
        }

        let now = Instant::now();
        if now.duration_since(last_tick) >= Duration::from_secs(2) {
            let cnt = handoff.callback_count.load(Ordering::Relaxed);
            let smp = handoff.callback_samples.load(Ordering::Relaxed);
            let err = handoff.error_count.load(Ordering::Relaxed);
            let drp = dropped_samples.load(Ordering::Relaxed);
            let dt = now.duration_since(last_tick).as_secs_f64();
            let cb_delta = cnt - last_cb_count;
            let smp_delta = smp - last_cb_samples;
            // Observed callback period (in ms). When buffer_size is
            // `Default`, this is the only way to know what the backend
            // actually negotiated.
            let period_ms = if cb_delta > 0 {
                let frames_per_cb = (smp_delta / ch as u64) as f64 / cb_delta as f64;
                frames_per_cb * 1000.0 / TARGET_RATE as f64
            } else {
                0.0
            };
            let ring_used_pct = ring_high_water as f64 * 100.0 / ring_cap as f64;
            log(&format!(
                "[capture] tick: cbs={cnt} (+{cb_delta} in {dt:.1}s, {cb_per_sec:.1}/s), samples={smp} ({smp_per_sec:.0} Sa/s ≈ {ratio:.1}× target), period={period_ms:.1}ms/cb, errors={err} (+{ed}), dropped={drp} mono-samples (+{dd}), ring_high_water={hw}B ({hw_pct:.1}% of {ring_cap}B)",
                cb_per_sec = cb_delta as f64 / dt,
                smp_per_sec = smp_delta as f64 / dt,
                ratio = smp_delta as f64 / (TARGET_RATE as f64 * ch as f64 * dt),
                ed = err - last_err_count,
                dd = drp - last_dropped,
                hw = ring_high_water,
                hw_pct = ring_used_pct,
            ));
            last_tick = now;
            last_cb_count = cnt;
            last_cb_samples = smp;
            last_err_count = err;
            last_dropped = drp;
            ring_high_water = 0;
        }
    }
    log("[capture-reader] stop flag → exit");
}

/// Decode interleaved native-format bytes into f32 mono (channel 0 only),
/// appending to `out`. Bytes are read via `from_ne_bytes` so the decode
/// is alignment-agnostic — the rtrb consumer hands us `&[u8]` slices
/// whose starting address has no f32/i16/u16 alignment guarantee.
///
/// `from_ne_bytes` is a no-op on every supported target (all little-endian
/// in practice: x86_64, aarch64, armv7l), so this compiles to the same
/// thing as a direct typed read on aligned data.
fn decode_native_to_mono_f32(
    bytes: &[u8],
    format: SampleFormat,
    channels: usize,
    out: &mut Vec<f32>,
) {
    match format {
        SampleFormat::F32 => {
            // Stride = bytes_per_sample × channels; we keep frame[0..4]
            // (the first channel) and skip the rest.
            let stride = 4 * channels;
            out.reserve(bytes.len() / stride);
            for frame in bytes.chunks_exact(stride) {
                let v = f32::from_ne_bytes([frame[0], frame[1], frame[2], frame[3]]);
                out.push(v);
            }
        }
        SampleFormat::I16 => {
            const SCALE: f32 = 1.0 / 32768.0;
            let stride = 2 * channels;
            out.reserve(bytes.len() / stride);
            for frame in bytes.chunks_exact(stride) {
                let v = i16::from_ne_bytes([frame[0], frame[1]]);
                out.push(v as f32 * SCALE);
            }
        }
        SampleFormat::U16 => {
            const OFFSET: f32 = 32768.0;
            const SCALE: f32 = 1.0 / 32768.0;
            let stride = 2 * channels;
            out.reserve(bytes.len() / stride);
            for frame in bytes.chunks_exact(stride) {
                let v = u16::from_ne_bytes([frame[0], frame[1]]);
                out.push((v as f32 - OFFSET) * SCALE);
            }
        }
        SampleFormat::U8 => {
            const OFFSET: f32 = 128.0;
            const SCALE: f32 = 1.0 / 128.0;
            let stride = channels;
            out.reserve(bytes.len() / stride);
            for frame in bytes.chunks_exact(stride) {
                out.push((frame[0] as f32 - OFFSET) * SCALE);
            }
        }
        _ => {} // unreachable: pre-validated upstream
    }
}

/// Best-effort RT-priority lift for the reader thread on Linux.
///
/// `SCHED_FIFO` priority 5 is enough to preempt CFS-scheduled noise
/// (browser tabs, cargo, the Tauri webview) but stays well below
/// PulseAudio (typically 5-50), kernel audio IRQs, and the system
/// watchdog. Requires either `CAP_SYS_NICE` or membership in the
/// `audio` group with a matching entry in `/etc/security/limits.conf`:
///
/// ```text
/// @audio   -   rtprio   95
/// ```
///
/// On a typical Debian / Raspberry Pi OS install the `audio` group is
/// pre-configured this way, and the default user (`pi`) belongs to it,
/// so the lift succeeds out of the box on a fresh image.
///
/// If `sched_setscheduler` fails (typical: unprivileged user not in
/// `audio`), we log once and proceed at normal priority. The 30 s ring
/// is wide enough to absorb CFS jitter on any modern Linux, so the lift
/// is purely a high-load optimisation, not a correctness requirement.
#[cfg(target_os = "linux")]
fn lift_rt_priority() -> bool {
    use libc::{sched_param, sched_setscheduler, SCHED_FIFO};
    let param = sched_param { sched_priority: 5 };
    // SAFETY: sched_setscheduler is a thread-local syscall (PID 0 means
    // "current thread"); `param` lives for the duration of the call.
    let rc = unsafe { sched_setscheduler(0, SCHED_FIFO, &param as *const _) };
    rc == 0
}

#[cfg(not(target_os = "linux"))]
fn lift_rt_priority() -> bool {
    // Windows would use SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL);
    // macOS would use pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE).
    // The 30 s ring is already wider than any realistic CFS / scheduler
    // hiccup on a modern OS, so this is a future optimisation, not a
    // correctness gap.
    false
}

/// Builds a throttled error callback for cpal `build_input_stream`.
///
/// cpal forwards every backend error verbatim, and on Linux/ALSA a
/// `POLLERR` condition can refire on every poll iteration — easily a
/// million times per second. The naive "log every error" approach
/// produced multi-GiB logs in production, so we count errors via the
/// shared atomic and only emit a line on power-of-ten milestones (and
/// once per million afterwards). The current count is also exposed in
/// the reader thread's tick log, so a chronically failing stream stays
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
