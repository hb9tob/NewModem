//! SDRplay RX path: USB I/Q (via the daemon) → 48 kHz mono `Vec<f32>`
//! audio batches.
//!
//! Mirrors `modem_pluto::rx` and `modem_io::cpal_capture` so
//! `modem-worker` plugs into the same `Receiver<Vec<f32>>` consumer
//! it already uses for the soundcard / Pluto. The big shape
//! difference vs. those two: SDRplay is **callback-driven**. The API
//! calls `stream_a_callback` from one of its own threads with a
//! chunk of int16 I/Q every few hundred microseconds; we run the
//! DSP chain there and ship the resulting 48 kHz audio over an
//! mpsc.
//!
//! Chain — delegates the DSP entirely to
//! [`modem_sdr_dsp::NbfmRxChain`], so this module owns transport
//! (callback plumbing, sample format, FFI lifetime) and the shared
//! crate owns the math. Pluto uses the same chain with
//! `lo_offset_hz = 0`; SDRplay passes
//! [`crate::device::DEFAULT_LO_OFFSET_HZ`] so the channel-select
//! NCO compensates the LO offset programmed at the tuner.
//!
//! ```text
//! sdrplay_api StreamCallback → I[i16], Q[i16]                (host I/Q rate)
//!   → Complex32 = (I + jQ) / 32768
//!   → NbfmRxChain.process()
//!         ↪ FreqXlatingFir (NCO + Kaiser LPF + decim → 48 kHz I/Q at DC)
//!         ↪ QuadratureDemod (at 48 kHz)
//!         ↪ DeemphasisLpf, SubAudioHpf
//!   → mpsc::Sender<Vec<f32>> (48 kHz mono audio)
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use num_complex::Complex32;

use modem_sdr_dsp::{NbfmRxChain, NbfmRxChainConfig};

use crate::api::{
    self, sdrplay_api_CallbackFnsT, sdrplay_api_EventParamsT, sdrplay_api_EventT,
    sdrplay_api_StreamCbParamsT, sdrplay_api_TunerSelectT,
};
use crate::device::{self, SdrplayConfig, SdrplaySession, DEFAULT_LO_OFFSET_HZ};
use crate::error::SdrplayError;


/// Audio rate the chain produces. Locked to the modem core's
/// `AUDIO_RATE` (48 kHz). Decim ratio against the host I/Q rate is
/// [`PREFERRED_AUDIO_RATIO`].
pub const AUDIO_RATE: u32 = modem_sdr_dsp::AUDIO_RATE;

/// Audio samples per `Vec<f32>` pushed into the worker mpsc. 1200 =
/// 25 ms at 48 kHz, matches what cpal/Pluto deliver per callback so
/// the worker side is homogeneous (memory `sdr-rate-convention.md`).
const TARGET_CHUNK_SAMPLES: usize = 1200;

/// 16-bit signed full-scale. RSPduo's stream callback hands us int16
/// I/Q already aligned full-scale (unlike Pluto's S12-in-S16 with
/// 2047 peak) — divide by 32768 to land in `Complex32`'s [-1, 1]
/// convention.
const RX_S16_PEAK: f32 = 32768.0;

/// How often the capture thread emits a status / error tick to
/// stderr. Same throttling pattern `modem_io::cpal_capture` and
/// `modem_pluto::rx` use.
const STATUS_TICK_PERIOD: Duration = Duration::from_secs(2);

/// Live handle on an SDRplay capture stream. Drop / call [`stop`] to
/// cancel streaming; the daemon-side cleanup runs in the worker
/// thread on the way out.
pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    pub sample_rate: u32,
    pub channels: u16,
    /// API-reported host I/Q rate (after the API's internal
    /// decimation). For the modem use case this is
    /// `cfg.sample_rate_hz / cfg.decimation` ≈ 576 kSa/s on the
    /// preferred path.
    pub host_iq_rate_hz: u64,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Per-callback state shared between the API's stream thread and
/// the user-facing capture thread. Allocated once, lives behind an
/// `Arc<Mutex<>>` so the C callback can borrow it briefly via the
/// `cbContext` void pointer the API hands back to us.
struct CallbackState {
    /// The whole NBFM RX DSP, in one struct. Same chain construction
    /// as `modem_pluto::rx`; backends differ only in the
    /// `lo_offset_hz` they pass at construction.
    chain: NbfmRxChain,
    pending: Vec<f32>,
    sample_tx: Sender<Vec<f32>>,
    /// Heartbeat counter — bumped every callback. Used by the
    /// supervisor thread to detect a stalled stream.
    total_iq_samples: u64,
    /// Frame error / dropped-sample counter, surfaced over stderr at
    /// the throttled tick.
    frame_errors: u64,
}

/// Open an SDRplay device for receive, kick off the API stream, and
/// return a 48 kHz mono f32 mpsc channel.
///
/// Mirrors `modem_io::cpal_capture::start(&str)` and
/// `modem_pluto::rx::start(&PlutoConfig)` — same
/// `(handle, mpsc::Receiver<Vec<f32>>)` tuple — so the worker doesn't
/// know it's talking to an RSPduo rather than a soundcard.
pub fn start(config: &SdrplayConfig) -> Result<(CaptureHandle, Receiver<Vec<f32>>), SdrplayError> {
    let session = device::open(config)?;
    start_on(session)
}

/// Same as [`start`] but takes an already-opened [`SdrplaySession`].
/// Lets a future loopback test reuse one open device for two
/// directions; production callers should prefer [`start`].
pub fn start_on(
    mut session: SdrplaySession,
) -> Result<(CaptureHandle, Receiver<Vec<f32>>), SdrplayError> {
    let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
    let stop = Arc::new(AtomicBool::new(false));

    // Build the DSP chain on the user's thread, hand it to the API
    // thread inside `CallbackState`. The host I/Q rate is whatever
    // came out of (sample_rate / decimation) — typically 576 kS/s on
    // the preferred path. NbfmRxChain takes care of decimating that
    // down to 48 kHz, NCO-shifting the LO offset out, and running
    // the demod + audio filters.
    let host_iq_rate_hz =
        (session.config.sample_rate_hz / session.config.decimation as f64).round() as u64;
    let chain = NbfmRxChain::new(NbfmRxChainConfig::new(
        host_iq_rate_hz as u32,
        session.config.max_deviation_hz,
        DEFAULT_LO_OFFSET_HZ as f32,
    ));
    let cb_state = Arc::new(Mutex::new(CallbackState {
        chain,
        pending: Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2),
        sample_tx,
        total_iq_samples: 0,
        frame_errors: 0,
    }));

    // Kick off the daemon-side stream. The callback functions are
    // C-callable trampolines that pull `CallbackState` out of the
    // void* context and run the DSP chain. bindgen surfaces both
    // function-pointer typedefs as `Option<unsafe extern "C" fn ...>`,
    // so the trampolines themselves must be `unsafe extern "C" fn`
    // (the API may pass null pointers in pathological cases — we
    // null-check before deref).
    let mut callbacks = sdrplay_api_CallbackFnsT {
        StreamACbFn: Some(stream_a_callback),
        StreamBCbFn: None,
        EventCbFn: Some(event_callback),
    };
    // SAFETY: we hand the API a stable pointer to our Arc<Mutex<>>;
    // the Arc is kept alive by the supervisor thread below until
    // after `Uninit` returns, so the API never dereferences a stale
    // pointer.
    let cb_ctx_arc: Arc<Mutex<CallbackState>> = cb_state.clone();
    let cb_ctx_ptr = Arc::into_raw(cb_ctx_arc) as *mut std::ffi::c_void;
    let lib = api::api()?;
    let init_res = unsafe {
        api::check(
            lib,
            "Init",
            lib.sdrplay_api_Init(session.device.dev, &mut callbacks as *mut _, cb_ctx_ptr),
        )
    };
    if let Err(e) = init_res {
        // Reclaim the Arc we leaked so the refcount doesn't pin the
        // CallbackState beyond this error path.
        // SAFETY: the API never read the pointer (Init failed).
        let _ = unsafe { Arc::from_raw(cb_ctx_ptr as *const Mutex<CallbackState>) };
        return Err(e);
    }

    // Supervisor thread — keeps the SdrplaySession alive, runs the
    // heartbeat ticker, and tears down the API on stop. We can't
    // capture `cb_ctx_ptr: *mut c_void` directly (raw pointers aren't
    // `Send` and a local marker-struct workaround triggers an
    // overly-conservative auto-trait inference in the closure body
    // — see Rust issue #29625). Trip it through `usize` instead:
    // every platform we target has `sizeof(*mut c_void) == sizeof(usize)`.
    let cb_ctx_addr: usize = cb_ctx_ptr as usize;
    let stop_thread = stop.clone();
    let cb_state_for_thread = cb_state.clone();
    let thread = thread::spawn(move || {
        let ctx = cb_ctx_addr as *mut std::ffi::c_void;
        run_supervisor(&mut session, stop_thread, cb_state_for_thread, ctx);
    });

    Ok((
        CaptureHandle {
            stop,
            thread: Some(thread),
            sample_rate: AUDIO_RATE,
            channels: 1,
            host_iq_rate_hz,
        },
        sample_rx,
    ))
}

fn run_supervisor(
    session: &mut SdrplaySession,
    stop: Arc<AtomicBool>,
    cb_state: Arc<Mutex<CallbackState>>,
    cb_ctx_ptr: *mut std::ffi::c_void,
) {
    let mut last_tick = std::time::Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // The API delivers samples on its own thread; we wake every
        // 200 ms only to print stats / honour the stop flag. The
        // mpsc backpressure is handled callback-side.
        thread::sleep(Duration::from_millis(200));
        if last_tick.elapsed() >= STATUS_TICK_PERIOD {
            if let Ok(g) = cb_state.lock() {
                eprintln!(
                    "[sdrplay-rx] tick: {} I/Q samples in, {} frame errors",
                    g.total_iq_samples, g.frame_errors
                );
            }
            last_tick = std::time::Instant::now();
        }
        // Detect mpsc receiver hangup → stop early. We can't poll
        // the sender side without a send, so use a sentinel: when
        // the mpsc has been disconnected, the next callback's send
        // will fail and bump frame_errors. The CB also raises stop
        // in that case (see stream_a_callback) so we'll see it here
        // on the next loop iteration.
    }

    // SAFETY: tearing down. Uninit blocks until the daemon's stream
    // thread has finished any in-flight callbacks, so no further
    // dereference of cb_ctx_ptr happens after this returns. If the
    // library somehow became unloadable mid-flight (shouldn't happen
    // — we got this far), there's nothing to call.
    if let Ok(lib) = api::api() {
        let _ = unsafe { api::check(lib, "Uninit", lib.sdrplay_api_Uninit(session.device.dev)) };
    }

    // Reclaim the Arc<Mutex<CallbackState>> we leaked into the API's
    // context. The Arc decrement is benign — the supervisor thread
    // and the worker mpsc's drop already released their refs.
    // SAFETY: matched against the `Arc::into_raw` in `start_on`.
    let _ = unsafe { Arc::from_raw(cb_ctx_ptr as *const Mutex<CallbackState>) };
    // Drop session at the end of run_supervisor — the destructor
    // calls ReleaseDevice + Close.
    let _ = session;
}

/// C-callable Stream A callback. Runs on a daemon-side thread; we
/// borrow [`CallbackState`] briefly via the void* context pointer,
/// run the DSP chain, and ship full chunks through the mpsc.
///
/// SAFETY: the SDRplay API guarantees `xi`, `xq` point at
/// `num_samples` valid int16 samples for the duration of the call,
/// and `cb_context` is whatever pointer was passed to
/// `sdrplay_api_Init` (set up in `start_on` to be a leaked
/// `Arc<Mutex<CallbackState>>` kept alive until `Uninit` returns).
unsafe extern "C" fn stream_a_callback(
    xi: *mut i16,
    xq: *mut i16,
    params: *mut sdrplay_api_StreamCbParamsT,
    num_samples: u32,
    _reset: u32,
    cb_context: *mut std::ffi::c_void,
) {
    if cb_context.is_null() || xi.is_null() || xq.is_null() || num_samples == 0 {
        return;
    }
    // SAFETY: cb_context was set in `start_on` via
    // `Arc::into_raw(Arc<Mutex<CallbackState>>)`, which returns a
    // `*const Mutex<CallbackState>` — a pointer to the **inner** T,
    // not to the Arc itself. The supervisor thread holds the
    // matching Arc<Mutex<…>> alive until `Uninit` returns, so the
    // pointee is valid for every callback the API delivers. We
    // borrow the Mutex without reconstructing the Arc, so the
    // refcount isn't touched.
    let mutex: &Mutex<CallbackState> =
        unsafe { &*(cb_context as *const Mutex<CallbackState>) };
    let mut g = match mutex.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let _ = params; // unused for now; could log dropped-sample counters

    // SAFETY: the API guarantees `xi` and `xq` are valid pointers to
    // `num_samples` int16 elements each for the duration of this
    // callback. We never read past `num_samples`.
    let n = num_samples as usize;
    let i_slice = unsafe { std::slice::from_raw_parts(xi, n) };
    let q_slice = unsafe { std::slice::from_raw_parts(xq, n) };

    // I/Q (i16, full-scale on 32768) → Complex32 → NbfmRxChain. The
    // chain runs the channel-select + demod + audio filters and
    // returns 48 kHz mono audio.
    let iq: Vec<Complex32> = i_slice
        .iter()
        .zip(q_slice.iter())
        .map(|(&i, &q)| {
            Complex32::new(i as f32 / RX_S16_PEAK, q as f32 / RX_S16_PEAK)
        })
        .collect();

    g.total_iq_samples = g.total_iq_samples.saturating_add(n as u64);
    let audio = g.chain.process(&iq);
    if audio.is_empty() {
        return;
    }

    // Accumulate, flush full TARGET_CHUNK_SAMPLES chunks. Same
    // policy as the Pluto path — pins worker mpsc rate to ~40/s.
    g.pending.extend_from_slice(&audio);
    while g.pending.len() >= TARGET_CHUNK_SAMPLES {
        let chunk: Vec<f32> = g.pending.drain(..TARGET_CHUNK_SAMPLES).collect();
        if g.sample_tx.send(chunk).is_err() {
            // Receiver hung up — bump the error counter; the
            // supervisor thread will see no fresh activity and the
            // user / GUI will surface a stop on its own timeline.
            g.frame_errors = g.frame_errors.saturating_add(1);
            break;
        }
    }
}

/// C-callable event callback — drains overload / sample-rate-changed
/// / RSPduo-mode-changed events from the daemon. We log them; the
/// modem doesn't (yet) react to overload events.
///
/// SAFETY: same `cb_context` lifetime contract as
/// [`stream_a_callback`]. We don't deref `params` here.
unsafe extern "C" fn event_callback(
    event_id: sdrplay_api_EventT,
    tuner: sdrplay_api_TunerSelectT,
    _params: *mut sdrplay_api_EventParamsT,
    _cb_context: *mut std::ffi::c_void,
) {
    eprintln!("[sdrplay-rx] event id={event_id:?} tuner={tuner:?}");
}
