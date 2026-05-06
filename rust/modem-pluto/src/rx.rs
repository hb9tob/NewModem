//! Pluto RX path: USB I/Q → 48 kHz mono `Vec<f32>` audio batches.
//!
//! Mirrors the shape of [`modem_io::cpal_capture`] so `modem-worker`
//! can plug a Pluto into the same `Receiver<Vec<f32>>` consumer it
//! already uses for the soundcard. The capture thread owns the libiio
//! `Buffer`, runs the radio-faithful DSP chain straight after the
//! buffer pump, and forwards 48 kHz mono `Vec<f32>` batches over an
//! mpsc.
//!
//! Chain (matches `modem_sdr_dsp`'s integration test layout):
//!
//! ```text
//! cf-ad9361-lpc → I[i16], Q[i16]                            (576 kHz IF)
//!   → Complex32 = (I + jQ) / 2048                           (S12-aligned)
//!   → QuadratureDemod                                       → real f32
//!   → PolyphaseDecimator (÷12 to 48 kHz)
//!   → DeemphasisLpf  (single-pole IIR, 300 Hz corner)
//!   → SubAudioHpf    (CTCSS-reject mirror)
//!   → mpsc::Sender<Vec<f32>>
//! ```
//!
//! The thread starts by re-running [`device::open`] inside itself —
//! that way the (`!Send`) intermediate `Channel` handles never leave
//! the worker, and `start()` only has to ship a `PlutoConfig` (which
//! is `Clone + Send`) across the spawn boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use num_complex::Complex32;

use modem_sdr_dsp::audio_filters::{DeemphasisLpf, SubAudioHpf};
use modem_sdr_dsp::decimator::PolyphaseDecimator;
use modem_sdr_dsp::fm_demod::QuadratureDemod;

use crate::device::{self, NegotiatedRate, PlutoConfig};
use crate::error::PlutoError;

/// Audio rate the chain produces. Locked to the modem core's
/// `AUDIO_RATE`. Decimation ratio against the AD9361's IF rate is
/// [`crate::PREFERRED_RATIO`] (or [`crate::FALLBACK_RATIO`] on
/// fallback).
pub const AUDIO_RATE: u32 = modem_sdr_dsp::AUDIO_RATE;

/// libiio buffer size, in I/Q samples. At 576 kHz, 8192 samples is
/// ~14 ms of latency per refill — small enough that the GUI feels
/// responsive but big enough that USB scheduling jitter doesn't
/// starve the buffer. Multiple of 16 because libiio's
/// `length_align_bytes` on `cf-ad9361-lpc` reads 8 (= 2 channels × 4
/// bytes per i16 pair, rounded).
const RX_BUFFER_SAMPLES: usize = 8192;

/// Audio samples per `Vec<f32>` pushed into the worker mpsc. 1200
/// samples = 25 ms at 48 kHz, which matches what the cpal soundcard
/// backend delivers per callback (measured ~40 callbacks/s, ~1200
/// samples each on the Pi 5 PulseAudio path). One refill of
/// [`RX_BUFFER_SAMPLES`] produces ~683 audio samples after ÷12
/// decimation, so we accumulate 2-3 refills' worth before flushing.
/// Equalising the chunk shape between cpal and Pluto keeps the
/// worker side homogeneous: same per-batch overhead, same mpsc
/// pressure, no special case.
const TARGET_CHUNK_SAMPLES: usize = 1200;

/// AD9361 / AD9363 RX path on Pluto outputs S12 in S16 LE — a 12-bit
/// signed sample sign-extended into a 16-bit word, peak ±2047. We
/// scale to unit-amplitude `Complex32` by dividing by this constant.
const RX_S12_PEAK: f32 = 2047.0;

/// How often the capture thread emits a status / error tick to stderr.
/// Mirrors the throttling pattern from `modem_io::cpal_capture` (commit
/// bab4311) so a USB hiccup doesn't flood the terminal.
const STATUS_TICK_PERIOD: Duration = Duration::from_secs(2);

/// Live handle on a Pluto capture thread. Drop / call [`stop()`] to
/// cancel streaming.
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

/// Open a Pluto for receive, kick off the capture thread, and return a
/// channel of 48 kHz mono f32 batches.
///
/// The signature mirrors `modem_io::cpal_capture::start(&str)` — same
/// `(handle, mpsc::Receiver<Vec<f32>>)` tuple — so the worker doesn't
/// know it's talking to an SDR rather than a soundcard.
pub fn start(config: &PlutoConfig) -> Result<(CaptureHandle, Receiver<Vec<f32>>), PlutoError> {
    let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<NegotiatedRate, PlutoError>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let cfg = config.clone();

    let thread = thread::spawn(move || {
        run_capture(cfg, sample_tx, ready_tx, stop_thread);
    });

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(rate)) => Ok((
            CaptureHandle {
                stop,
                thread: Some(thread),
                sample_rate: AUDIO_RATE,
                channels: 1,
                negotiated_iq_rate_hz: rate.sample_rate_hz,
            },
            sample_rx,
        )),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(PlutoError::Stream(
            "timeout waiting for Pluto capture thread to start".into(),
        )),
    }
}

/// Same as [`start`] but takes an already-opened [`device::PlutoSession`].
/// Used by the loopback test where one open Pluto serves both
/// directions; production callers should prefer [`start`].
pub fn start_on(
    session: device::PlutoSession,
    sample_tx: Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<NegotiatedRate, PlutoError> {
    capture_loop(session, sample_tx, stop)
}

fn run_capture(
    config: PlutoConfig,
    sample_tx: Sender<Vec<f32>>,
    ready_tx: Sender<Result<NegotiatedRate, PlutoError>>,
    stop: Arc<AtomicBool>,
) {
    let session = match device::open(&config) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let rate = session.negotiated_rate;
    let _ = ready_tx.send(Ok(rate));
    let _ = capture_loop(session, sample_tx, stop);
}

/// Inner loop. Returns once `stop` is set or the buffer pump fails.
/// Used both by [`run_capture`] and by [`start_on`].
fn capture_loop(
    session: device::PlutoSession,
    sample_tx: Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<NegotiatedRate, PlutoError> {
    use industrial_io::Direction;

    // Find the I and Q channels on the buffer-capable RX device.
    // cf-ad9361-lpc exposes voltage0 (I) and voltage1 (Q), both input.
    let i_chan = session
        .rx_buffer_dev
        .find_channel("voltage0", Direction::Input)
        .ok_or(PlutoError::Attribute {
            device: "cf-ad9361-lpc",
            attr: "voltage0 (I)",
            detail: "RX I channel not present".into(),
        })?;
    let q_chan = session
        .rx_buffer_dev
        .find_channel("voltage1", Direction::Input)
        .ok_or(PlutoError::Attribute {
            device: "cf-ad9361-lpc",
            attr: "voltage1 (Q)",
            detail: "RX Q channel not present".into(),
        })?;
    i_chan.enable();
    q_chan.enable();

    let mut buf = session
        .rx_buffer_dev
        .create_buffer(RX_BUFFER_SAMPLES, false)
        .map_err(|e| PlutoError::Stream(format!("create RX buffer: {e}")))?;

    // DSP chain — built once, reused on every refill.
    let if_rate = session.negotiated_rate.sample_rate_hz as f32;
    let ratio = session.negotiated_rate.ratio;
    let mut demod = QuadratureDemod::new(if_rate, modem_sdr_dsp::MAX_DEVIATION_HZ);
    // The decimator's input rate is the QuadratureDemod's output rate
    // (= the IF rate, post-discriminator audio is at the IF rate). The
    // taps are designed for an audio-band LPF at 24 kHz cutoff
    // (= AUDIO_RATE / 2), with a 99-tap Hamming sinc. 99 is odd so the
    // FIR stays linear-phase and centered.
    let dec_taps = PolyphaseDecimator::hamming_sinc_taps(if_rate, AUDIO_RATE as f32 / 2.0, 99);
    let mut decim = PolyphaseDecimator::with_taps(dec_taps, ratio);
    let mut deemph = DeemphasisLpf::new(AUDIO_RATE as f32, DeemphasisLpf::DEFAULT_CORNER_HZ);
    let mut hpf = SubAudioHpf::new(AUDIO_RATE as f32, SubAudioHpf::DEFAULT_CORNER_HZ);

    // Throttled-error state.
    let mut last_tick = std::time::Instant::now();
    let mut refill_errors: u64 = 0;
    let mut total_iq_samples: u64 = 0;

    // Aggregation buffer between refills — one libiio refill produces
    // ~RX_BUFFER_SAMPLES/ratio audio samples (~683 at 576 kHz ÷12),
    // smaller than [`TARGET_CHUNK_SAMPLES`]. We accumulate until we
    // have a full chunk, then flush. Pre-allocated for a couple of
    // chunks' worth so steady-state runs allocation-free.
    let mut pending: Vec<f32> = Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2);

    while !stop.load(Ordering::Relaxed) {
        let n = match buf.refill() {
            Ok(n) => n,
            Err(e) => {
                refill_errors += 1;
                if last_tick.elapsed() >= STATUS_TICK_PERIOD {
                    eprintln!("[pluto-rx] refill error #{refill_errors}: {e}");
                    last_tick = std::time::Instant::now();
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
        };
        if n == 0 {
            continue;
        }

        // Demux i16 I and Q from the same buffer. industrial-io's
        // `read::<i16>` does the demux + sign-extend (the AD9361 RX
        // path is S12 in S16 LE).
        let i_samples: Vec<i16> = i_chan
            .read::<i16>(&buf)
            .map_err(|e| PlutoError::Stream(format!("RX read I: {e}")))?;
        let q_samples: Vec<i16> = q_chan
            .read::<i16>(&buf)
            .map_err(|e| PlutoError::Stream(format!("RX read Q: {e}")))?;
        debug_assert_eq!(i_samples.len(), q_samples.len());
        total_iq_samples += i_samples.len() as u64;

        // Pair into Complex<f32>, scaled to unit amplitude.
        let iq: Vec<Complex32> = i_samples
            .iter()
            .zip(q_samples.iter())
            .map(|(&i, &q)| Complex32::new(i as f32 / RX_S12_PEAK, q as f32 / RX_S12_PEAK))
            .collect();

        // Discriminate at IF rate, then decimate.
        let mut audio_if = vec![0.0f32; iq.len()];
        demod.process(&iq, &mut audio_if);
        let mut audio = decim.process(&audio_if);
        if audio.is_empty() {
            continue;
        }

        // Radio-faithful audio post-processing.
        deemph.process(&mut audio);
        hpf.process(&mut audio);

        // Accumulate into `pending`, flush full chunks of
        // [`TARGET_CHUNK_SAMPLES`]. The remainder stays in `pending`
        // for the next refill — no samples dropped, no reframing
        // jitter visible to the worker. This drops the mpsc send
        // rate from ~70/s (per-refill) to ~40/s (per-chunk) which
        // matches cpal exactly.
        pending.extend_from_slice(&audio);
        let mut hung_up = false;
        while pending.len() >= TARGET_CHUNK_SAMPLES {
            let chunk: Vec<f32> = pending.drain(..TARGET_CHUNK_SAMPLES).collect();
            if sample_tx.send(chunk).is_err() {
                hung_up = true;
                break;
            }
        }
        if hung_up {
            break;
        }

        if last_tick.elapsed() >= STATUS_TICK_PERIOD {
            eprintln!(
                "[pluto-rx] tick: {total_iq_samples} I/Q samples in, {refill_errors} refill errors"
            );
            last_tick = std::time::Instant::now();
        }
    }
    Ok(session.negotiated_rate)
}
