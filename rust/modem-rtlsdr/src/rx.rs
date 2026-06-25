//! RTL-SDR RX path: USB u8 I/Q → 48 kHz mono `Vec<f32>` audio batches.
//!
//! Mirrors `modem_sdrplay::rx` and `modem_io::cpal_capture` so
//! `modem-worker` plugs into the same `Receiver<Vec<f32>>` consumer
//! it already uses for the soundcard / Pluto / SDRplay paths. The
//! shape difference vs cpal/Pluto: librtlsdr is **callback-driven** —
//! once `rtlsdr_read_async` is called, the librtlsdr internal USB
//! thread calls our trampoline with a chunk of u8 I/Q every few
//! milliseconds; we run the DSP chain there and ship the resulting
//! 48 kHz audio over an mpsc.
//!
//! Chain — delegates the DSP entirely to
//! [`modem_sdr_dsp::NbfmRxChain`], so this module owns transport
//! (callback plumbing, u8 → Complex32, FFI lifetime) and the shared
//! crate owns the math. SDRplay passes
//! `DEFAULT_LO_OFFSET_HZ = +75_000`; the RTL-SDR's R820T-family DC
//! artefact is wider, so we use `+250_000` (see
//! [`crate::device::DEFAULT_LO_OFFSET_HZ`]).
//!
//! ```text
//! librtlsdr USB thread → u8 buf (interleaved I, Q, I, Q, …)
//!   → Complex32 = ((I - 127.5) / 128.0) + j((Q - 127.5) / 128.0)
//!   → NbfmRxChain.process()
//!         ↪ FreqXlatingFir (NCO + Kaiser LPF + decim ÷48 → 48 kHz IQ)
//!         ↪ QuadratureDemod
//!         ↪ DeemphasisLpf, SubAudioHpf
//!   → mpsc::Sender<Vec<f32>> (48 kHz mono audio)
//! ```

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use num_complex::Complex32;

use modem_sdr::telemetry::{RadioCommand, RadioTelemetry};
use modem_sdr_dsp::{NbfmRxChain, NbfmRxChainConfig};
use modem_sdr_radio::{RadioInit, RadioRuntime};

use crate::device::{
    self, RtlsdrSession, DEFAULT_LO_OFFSET_HZ, READ_BUF_LEN, READ_BUF_NUM,
};
use crate::error::RtlsdrError;
use crate::ffi::{rtlsdr_dev_t, RtlsdrLib};
use crate::hardware::RtlHardware;

/// The two channel ends the backend hands the capture path to drive the
/// Radio tab: telemetry out (spectra / S-meter / tune state) and control in
/// (retune / gain / squelch). `None` for plain captures.
pub struct RadioWiring {
    pub telemetry_tx: Sender<RadioTelemetry>,
    pub control_rx: Receiver<RadioCommand>,
}

/// Audio rate the chain produces. Locked to the modem core's
/// `AUDIO_RATE` (48 kHz).
pub const AUDIO_RATE: u32 = modem_sdr_dsp::AUDIO_RATE;

/// Audio samples per `Vec<f32>` pushed into the worker mpsc. 1200 =
/// 25 ms at 48 kHz, matches what cpal / Pluto / SDRplay deliver per
/// callback so the worker side is homogeneous.
const TARGET_CHUNK_SAMPLES: usize = 1200;

/// u8 I/Q centre point. The RTL2832U's 8-bit ADC has mid-scale at
/// 127.5; subtract that to recentre, then divide by 128 to land in
/// `Complex32`'s [-1, 1] convention. The 0.5 is real, not an
/// off-by-one — every published librtlsdr-using app does the same
/// (see `rtl_fm.c`, `gqrx`, …).
const U8_BIAS: f32 = 127.5;
const U8_SCALE: f32 = 128.0;

/// Live handle on an RTL-SDR capture stream. Drop / call [`stop`] to
/// cancel streaming; the librtlsdr-side cleanup runs in the worker
/// thread on the way out.
pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Live host I/Q rate from the dongle (= `cfg.sample_rate_hz`).
    pub host_iq_rate_hz: u64,
    /// Radio-tab control thread (drains retune / gain commands). `None`
    /// for plain captures. Joined **before** the capture thread on
    /// teardown so it stops issuing tuner FFI before the device closes.
    control_thread: Option<JoinHandle<()>>,
    /// Raw device pointer so we can call `rtlsdr_cancel_async` from
    /// outside the capture thread on Drop. SAFETY: only ever invoked
    /// once, before the join; the cancel call wakes the blocking
    /// `read_async` so the thread can exit and the device be closed.
    cancel_handle: CancelHandle,
}

/// Newtype around the raw device pointer so we can carry it across
/// the thread boundary safely. The handle is only used to call
/// `rtlsdr_cancel_async` — never to dereference fields of the
/// device struct.
struct CancelHandle(*mut rtlsdr_dev_t);
// SAFETY: `rtlsdr_cancel_async` is thread-safe per librtlsdr's
// design (it's the documented way to interrupt `rtlsdr_read_async`
// from a different thread).
unsafe impl Send for CancelHandle {}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.teardown();
    }

    /// Idempotent teardown. Order matters: the control thread must stop
    /// issuing tuner FFI *before* the capture thread closes the device, so
    /// we join it first (it exits within one 200 ms poll of `stop`), then
    /// wake + join the capture thread which owns `rtlsdr_close`.
    fn teardown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.control_thread.take() {
            let _ = h.join();
        }
        // Wake the blocking read_async so the capture thread can teardown.
        // cancel_async is safe to call even after the stream has ended.
        if let Ok(lib) = RtlsdrLib::get() {
            // SAFETY: cancel_async accepts the live dev pointer; we only
            // call this once before joining the capture thread.
            unsafe {
                let _ = (lib.cancel_async)(self.cancel_handle.0);
            }
        }
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Per-callback state shared between the librtlsdr USB thread and
/// `CaptureHandle`. Allocated once, lives behind an `Arc<Mutex<>>` so
/// the C callback can borrow it via the `cb_context` void pointer.
struct CallbackState {
    chain: NbfmRxChain,
    pending: Vec<f32>,
    sample_tx: Sender<Vec<f32>>,
    /// Heartbeat counter — bumped every callback so a future
    /// supervisor can detect a stalled stream. Surfaced over stderr
    /// at the throttled tick below.
    total_iq_samples: u64,
    /// mpsc-send failures (receiver hung up).
    frame_errors: u64,
    /// Tracks when we last printed the heartbeat line.
    last_tick: std::time::Instant,
    /// Reuse this scratch to avoid reallocating the Complex32 buffer
    /// per callback (the same chunk size keeps coming back).
    iq_scratch: Vec<Complex32>,
    /// Radio-tab runtime (RF/audio spectra, S-meter, tune-command apply).
    /// `None` for plain captures. Shared between the USB callback (on_iq /
    /// on_audio) and the control thread (apply_command), both behind this
    /// struct's mutex.
    radio: Option<RadioRuntime>,
}

impl CallbackState {
    /// Apply the DSP side of a Radio-tab command to the chain (no-op
    /// without radio). Called from the control thread under this mutex; the
    /// hardware side runs separately, outside the lock.
    fn apply_radio_command(&mut self, cmd: RadioCommand) {
        if let Some(rt) = self.radio.as_mut() {
            rt.apply_command(cmd, &mut self.chain);
        }
    }
}

const STATUS_TICK_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

/// Open an RTL-SDR device for receive, kick off the async USB stream,
/// and return a 48 kHz mono f32 mpsc channel. Mirrors
/// `modem_io::cpal_capture::start(&str)` and
/// `modem_sdrplay::rx::start(&SdrplayConfig)` — same
/// `(handle, mpsc::Receiver<Vec<f32>>)` tuple — so the worker doesn't
/// know it's talking to an RTL-SDR rather than a soundcard.
pub fn start(
    config: &device::RtlsdrConfig,
) -> Result<(CaptureHandle, Receiver<Vec<f32>>), RtlsdrError> {
    let session = device::open(config)?;
    start_on(session, None)
}

/// Same as [`start`] but takes an already-opened [`RtlsdrSession`] and an
/// optional [`RadioWiring`] (Radio-tab telemetry + control). `None` gives a
/// plain capture; `Some` wires the shared [`RadioRuntime`] and drives live
/// retune / gain from a dedicated control thread (never the USB callback).
pub fn start_on(
    session: RtlsdrSession,
    radio: Option<RadioWiring>,
) -> Result<(CaptureHandle, Receiver<Vec<f32>>), RtlsdrError> {
    let lib = RtlsdrLib::get()?;
    let host_iq_rate_hz = session.config.sample_rate_hz as u64;

    let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
    let stop = Arc::new(AtomicBool::new(false));

    let chain = NbfmRxChain::new(NbfmRxChainConfig::new(
        host_iq_rate_hz as u32,
        session.config.max_deviation_hz,
        DEFAULT_LO_OFFSET_HZ as f32,
    ));

    // Build the Radio-tab runtime (if wired). Zero-IF model: the LO is
    // programmed `DEFAULT_LO_OFFSET_HZ` above the user frequency and the
    // chain's NCO already sits at `-offset`, so seed `digital_offset =
    // -offset` ⇒ `displayed_rf = user`. `control_rx` goes to the control
    // thread spawned below.
    let (radio_rt, control_rx) = match radio {
        Some(w) => {
            let init = RadioInit {
                input_rate_hz: host_iq_rate_hz as u32,
                lo_hz: session.config.rf_freq_hz + DEFAULT_LO_OFFSET_HZ as u64,
                digital_offset_hz: -(DEFAULT_LO_OFFSET_HZ as f64),
                lo_offset_hz: DEFAULT_LO_OFFSET_HZ as f32,
                max_deviation_hz: session.config.max_deviation_hz,
                dc_tunable: false,
            };
            (
                Some(RadioRuntime::new(w.telemetry_tx, init)),
                Some(w.control_rx),
            )
        }
        None => (None, None),
    };

    let cb_state = Arc::new(Mutex::new(CallbackState {
        chain,
        pending: Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2),
        sample_tx,
        total_iq_samples: 0,
        frame_errors: 0,
        last_tick: std::time::Instant::now(),
        iq_scratch: Vec::new(),
        radio: radio_rt,
    }));

    // Move the raw device pointer out of the session — the capture
    // thread owns teardown from here on (`rtlsdr_close` after the
    // blocking `read_async` returns).
    let dev = session.take_dev().ok_or_else(|| RtlsdrError::Stream(
        "session has no live device handle".into(),
    ))?;

    // Leak the Arc into a void* so the C callback can borrow the
    // CallbackState. Reclaimed inside the capture thread after
    // read_async returns, so the refcount drops to one and the
    // mutex's Drop runs cleanly.
    let cb_ctx_arc: Arc<Mutex<CallbackState>> = cb_state.clone();
    let cb_ctx_ptr = Arc::into_raw(cb_ctx_arc) as *mut c_void;

    // The capture thread can't directly hold the raw `*mut
    // rtlsdr_dev_t` (raw pointers aren't Send), so we route it via
    // usize (every platform we target has `sizeof(*mut T) ==
    // sizeof(usize)`). Same trick the SDRplay rx path uses for the
    // cb_context pointer.
    let dev_addr: usize = dev as usize;
    let cb_ctx_addr: usize = cb_ctx_ptr as usize;
    let stop_thread = stop.clone();
    // Drop the session here so its Drop impl doesn't try to close
    // the device — we've transferred ownership to the capture
    // thread via `dev_addr`.
    drop(session);

    let thread = thread::Builder::new()
        .name("rtlsdr-rx".into())
        .spawn(move || {
            let dev = dev_addr as *mut rtlsdr_dev_t;
            let cb_ctx = cb_ctx_addr as *mut c_void;
            run_capture(dev, cb_ctx, stop_thread);
        })
        .map_err(|e| {
            // Reclaim the Arc we leaked — the capture thread never
            // started, so the callback won't run.
            // SAFETY: matched against the Arc::into_raw above.
            let _ = unsafe {
                Arc::from_raw(cb_ctx_ptr as *const Mutex<CallbackState>)
            };
            // Close the device since we won't be using it.
            // SAFETY: dev came from rtlsdr_open and was never handed off.
            unsafe {
                let _ = (lib.close)(dev);
            }
            RtlsdrError::Stream(format!("spawn capture thread: {e}"))
        })?;

    // Radio-tab control thread: owns `control_rx` + a clone of the
    // CallbackState Arc + the device pointer (as usize, for tuner FFI). It
    // applies retune / gain off the USB callback thread (where re-entering
    // libusb would deadlock). Joined before the capture thread on teardown.
    let control_thread = control_rx.map(|rx| {
        let cb = cb_state.clone();
        let stop_ctl = stop.clone();
        thread::spawn(move || {
            let mut hw = RtlHardware::new(dev_addr, lib);
            run_control(rx, cb, stop_ctl, &mut hw);
        })
    });

    Ok((
        CaptureHandle {
            stop,
            thread: Some(thread),
            sample_rate: AUDIO_RATE,
            channels: 1,
            host_iq_rate_hz,
            control_thread,
            cancel_handle: CancelHandle(dev),
        },
        sample_rx,
    ))
}

/// Radio-tab control-thread body: drain retune / gain commands and apply
/// them under the CallbackState mutex. Exits within one 200 ms poll of the
/// `stop` flag, or immediately when every command sender has dropped.
fn run_control(
    control_rx: Receiver<RadioCommand>,
    cb_state: Arc<Mutex<CallbackState>>,
    stop: Arc<AtomicBool>,
    hw: &mut RtlHardware,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match control_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(cmd) => {
                // Hardware write OUTSIDE the lock: librtlsdr's USB callback
                // thread must keep processing events to complete the control
                // transfer, and it can't if it's blocked on this mutex.
                RadioRuntime::run_hardware(&cmd, hw);
                if let Ok(mut g) = cb_state.lock() {
                    g.apply_radio_command(cmd);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Capture-thread body: blocks in `rtlsdr_read_async` until the
/// CaptureHandle's Drop fires `rtlsdr_cancel_async`, then tears the
/// device down and reclaims the leaked Arc.
fn run_capture(dev: *mut rtlsdr_dev_t, cb_ctx: *mut c_void, _stop: Arc<AtomicBool>) {
    let lib = match RtlsdrLib::get() {
        Ok(l) => l,
        Err(_) => {
            // Library disappeared between start and now — extremely
            // unlikely; just reclaim the leaked Arc and bail.
            // SAFETY: matched against the Arc::into_raw in start_on.
            let _ = unsafe {
                Arc::from_raw(cb_ctx as *const Mutex<CallbackState>)
            };
            return;
        }
    };

    // SAFETY: dev is valid (just opened, never handed elsewhere); cb
    // is a valid `extern "C" fn`; cb_ctx is the leaked Arc pointer the
    // callback borrows on each invocation. read_async blocks until
    // cancel_async runs (or the device disconnects).
    let rc = unsafe {
        (lib.read_async)(
            dev,
            Some(stream_callback),
            cb_ctx,
            READ_BUF_NUM,
            READ_BUF_LEN,
        )
    };
    if rc != 0 {
        eprintln!("[rtlsdr-rx] read_async returned code={rc}");
    }

    // read_async returned — either the user cancelled, or the USB
    // bus errored out. Either way, close the device and reclaim the
    // callback context.
    // SAFETY: dev came from rtlsdr_open; matched close.
    unsafe {
        let _ = (lib.close)(dev);
    }
    // SAFETY: matched against Arc::into_raw in start_on. After
    // read_async returns, the callback can no longer be invoked.
    let _ = unsafe {
        Arc::from_raw(cb_ctx as *const Mutex<CallbackState>)
    };
}

/// C-callable callback librtlsdr invokes with each USB transfer.
///
/// SAFETY: `buf` points to `len` valid u8 bytes for the duration of
/// the call; `ctx` is the leaked `Arc<Mutex<CallbackState>>` pointer
/// the capture thread set up. We borrow the Mutex without
/// reconstructing the Arc so the refcount isn't touched.
unsafe extern "C" fn stream_callback(buf: *mut u8, len: u32, ctx: *mut c_void) {
    if buf.is_null() || ctx.is_null() || len < 2 {
        return;
    }
    // SAFETY: cb_ctx was set in start_on via Arc::into_raw(Arc<Mutex<...>>);
    // the capture thread holds the matching Arc alive until read_async
    // returns, so the pointee is valid for every callback.
    let mutex: &Mutex<CallbackState> = unsafe { &*(ctx as *const Mutex<CallbackState>) };
    let mut g = match mutex.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    // SAFETY: buf is valid for `len` bytes. We never read past it.
    let bytes = unsafe { std::slice::from_raw_parts(buf, len as usize) };
    let n_samples = bytes.len() / 2;

    // Destructure once so the borrow checker can prove that
    // `iq_scratch`, `chain`, `pending`, … are independent fields and
    // can be borrowed disjointly. Without this, every later access to
    // a field of `*g` overlaps with the `&mut g.iq_scratch` borrow.
    let CallbackState {
        chain,
        pending,
        sample_tx,
        total_iq_samples,
        frame_errors,
        last_tick,
        iq_scratch,
        radio,
    } = &mut *g;

    // Convert interleaved u8 I/Q → Complex32 into the reusable scratch.
    if iq_scratch.len() < n_samples {
        iq_scratch.resize(n_samples, Complex32::new(0.0, 0.0));
    }
    {
        let iq = &mut iq_scratch[..n_samples];
        for (k, pair) in bytes.chunks_exact(2).enumerate() {
            let i = (pair[0] as f32 - U8_BIAS) / U8_SCALE;
            let q = (pair[1] as f32 - U8_BIAS) / U8_SCALE;
            iq[k] = Complex32::new(i, q);
        }
    }

    *total_iq_samples = total_iq_samples.saturating_add(n_samples as u64);

    let now = std::time::Instant::now();
    if now.duration_since(*last_tick) >= STATUS_TICK_PERIOD {
        eprintln!(
            "[rtlsdr-rx] tick: {total_iq_samples} I/Q samples in, {frame_errors} frame errors"
        );
        *last_tick = now;
    }

    // Radio tab: wideband RF spectrum from the raw I/Q (before the chain).
    if let Some(rt) = radio.as_mut() {
        rt.on_iq(&iq_scratch[..n_samples]);
    }

    // chain.process needs &[Complex32]; iq_scratch is borrowed
    // mutably above. Read-only reborrow is enough since the mutating
    // pass is finished.
    let mut audio = chain.process(&iq_scratch[..n_samples]);
    if audio.is_empty() {
        return;
    }
    // Radio tab: S-meter + audio spectrum; mute the chunk under squelch.
    if let Some(rt) = radio.as_mut() {
        if rt.on_audio(
            &audio,
            chain.last_channel_power(),
            chain.last_excursion_peak(),
            chain.last_excursion_rms(),
        ) {
            audio.iter_mut().for_each(|s| *s = 0.0);
        }
    }

    pending.extend_from_slice(&audio);
    while pending.len() >= TARGET_CHUNK_SAMPLES {
        let chunk: Vec<f32> = pending.drain(..TARGET_CHUNK_SAMPLES).collect();
        if sample_tx.send(chunk).is_err() {
            *frame_errors = frame_errors.saturating_add(1);
            // Receiver hung up — bail out of this callback. The
            // CaptureHandle's Drop will fire cancel_async to wake the
            // outer read_async.
            return;
        }
    }
}
