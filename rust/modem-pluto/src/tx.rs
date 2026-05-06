//! Pluto TX path: 48 kHz mono audio `Vec<f32>` → USB I/Q at 576 / 2304 kSa/s.
//!
//! The contract mirrors `modem_io::traits::SampleSink` (one-shot full
//! buffer submit, returns a progress handle), but the impl block lives
//! in modem-worker (or in a thin glue module above this crate) so that
//! `modem-pluto` doesn't have to import `modem-io` and create a cycle.
//! See [`PlutoSink::play_buffer`] for the native entry point — that's
//! the one the integration tests and (eventual) modem-worker glue
//! call.
//!
//! Chain (matches `modem_sdr_dsp`'s integration test layout):
//!
//! ```text
//! Vec<f32> @ 48 kHz audio
//!   → PolyphaseInterpolator (×N to AD9361 IF rate)
//!   → PhaseMod (radio-faithful PM, +6 dB/oct preemph for free)
//!   → pack to S16/16 LE interleaved (peak ±32000 — leaves headroom
//!     under the AD9361 DAC ceiling so PM peaks don't clip)
//!   → libiio cf-ad9361-dds-core-lpc TX buffer
//! ```
//!
//! ## Why chunked streaming and not one big upload
//!
//! Worst-case math: 5 minutes of audio at 48 kHz = 14.4 M f32 samples
//! (57.6 MB). Interpolated to 528 kHz Complex<f32> that's 158 M
//! samples (1.27 GB). Packed S16 I/Q is 632 MB. We can't allocate
//! that up front and we don't want to. The TX worker loops over the
//! input audio in [`TX_CHUNK_AUDIO_SAMPLES`]-sized batches, runs each
//! batch through the chain, packs it to S16 I/Q, and pushes it to a
//! libiio buffer of [`TX_BUFFER_IQ_SAMPLES`] I/Q samples.
//! `Buffer::push()` blocks until the previous batch has been clocked
//! out, which gives natural backpressure against the AD9361 DAC.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use num_complex::Complex32;

use modem_sdr_dsp::interpolator::PolyphaseInterpolator;
use modem_sdr_dsp::pm_mod::PhaseMod;

use crate::device::{self, PlutoConfig};
use crate::error::PlutoError;

/// Audio batch size pulled from the input `Vec<f32>` per chain run.
/// 4096 samples = ~85 ms of audio at 48 kHz. The libiio TX buffer is
/// sized to hold exactly `TX_CHUNK_AUDIO_SAMPLES * ratio` I/Q samples
/// so each `push()` sends only modulated audio — no zero-padding,
/// which would otherwise inject silent gaps between chunks (the chip
/// clocks every sample in the buffer to its DAC, padding included).
const TX_CHUNK_AUDIO_SAMPLES: usize = 4096;

/// AD9361 / AD9363 TX path on Pluto consumes S16/16 LE — full int16
/// range. We scale unit-amplitude `Complex32` PM output by this
/// constant. PhaseMod is constant-envelope (|y| = 1) so peak input
/// is exactly 1.0; we leave a safety margin below `i16::MAX` so any
/// quantization noise doesn't clip.
const TX_S16_SCALE: f32 = 32_000.0;

/// Throttled-error tick.
const STATUS_TICK_PERIOD: Duration = Duration::from_secs(2);

/// `SampleSink`-shaped TX target for Pluto.
///
/// Cloneable so the caller can keep one for retune (LO / gain) while
/// the worker thread holds another for the buffer pump. Both clones
/// share the same Arc-counted libiio context.
#[derive(Clone, Debug)]
pub struct PlutoSink {
    config: PlutoConfig,
}

impl PlutoSink {
    pub fn new(config: PlutoConfig) -> Self {
        Self { config }
    }

    /// One-shot full-buffer submit. Spawns a worker thread that opens
    /// a Pluto context, configures it via [`device::open`], runs the
    /// upsample + PM chain on `audio_samples` in chunks, and pushes
    /// the resulting I/Q to `cf-ad9361-dds-core-lpc`. Returns a
    /// [`TxJob`] the caller polls / drops.
    ///
    /// Returns immediately after the first chunk has been pushed,
    /// matching `cpal_capture::start`'s ready-handshake pattern.
    pub fn play_buffer(&self, audio_samples: Vec<f32>) -> Result<TxJob, PlutoError> {
        let total_samples = audio_samples.len();
        let pos = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), PlutoError>>();

        let pos_thread = pos.clone();
        let stop_thread = stop.clone();
        let cfg = self.config.clone();

        let thread = thread::spawn(move || {
            let _ = run_tx(
                cfg,
                audio_samples,
                pos_thread,
                stop_thread,
                ready_tx,
            );
        });

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(TxJob {
                pos,
                total_samples,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(PlutoError::Stream(
                    "timeout starting Pluto TX thread".into(),
                ))
            }
        }
    }

    /// Run TX on an already-opened session — used by the loopback
    /// test where one Pluto serves both directions. Unlike
    /// [`Self::play_buffer`], this is **synchronous**: it returns
    /// after the entire `audio_samples` buffer has been pushed and
    /// drained. Caller spawns its own thread if it wants async.
    pub fn play_on(
        session: device::PlutoSession,
        audio_samples: &[f32],
        stop: Arc<AtomicBool>,
    ) -> Result<(), PlutoError> {
        run_tx_loop(session, audio_samples, &AtomicUsize::new(0), stop)
    }
}

/// Live handle on a TX job. Same pos / total_samples shape as
/// `modem_io::traits::PlaybackHandle` so the worker's progress code
/// stays unchanged.
pub struct TxJob {
    pub pos: Arc<AtomicUsize>,
    pub total_samples: usize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TxJob {
    pub fn pos(&self) -> usize {
        self.pos.load(Ordering::Relaxed).min(self.total_samples)
    }

    pub fn total_samples(&self) -> usize {
        self.total_samples
    }

    pub fn is_done(&self) -> bool {
        self.pos.load(Ordering::Relaxed) >= self.total_samples
    }

    /// Cancel any pending push and wait for the worker thread to exit.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TxJob {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn run_tx(
    config: PlutoConfig,
    audio: Vec<f32>,
    pos: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<(), PlutoError>>,
) -> Result<(), PlutoError> {
    let session = match device::open(&config) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return Ok(());
        }
    };
    let _ = ready_tx.send(Ok(()));
    run_tx_loop(session, &audio, &pos, stop)
}

fn run_tx_loop(
    session: device::PlutoSession,
    audio: &[f32],
    pos: &AtomicUsize,
    stop: Arc<AtomicBool>,
) -> Result<(), PlutoError> {
    use industrial_io::Direction;

    // Find I and Q output channels on the buffer-capable TX device.
    // cf-ad9361-dds-core-lpc exposes voltage0 (I) and voltage1 (Q),
    // both output, format S16/16 LE.
    let i_chan = session
        .tx_buffer_dev
        .find_channel("voltage0", Direction::Output)
        .ok_or(PlutoError::Attribute {
            device: "cf-ad9361-dds-core-lpc",
            attr: "voltage0 (I)",
            detail: "TX I channel not present".into(),
        })?;
    let q_chan = session
        .tx_buffer_dev
        .find_channel("voltage1", Direction::Output)
        .ok_or(PlutoError::Attribute {
            device: "cf-ad9361-dds-core-lpc",
            attr: "voltage1 (Q)",
            detail: "TX Q channel not present".into(),
        })?;
    i_chan.enable();
    q_chan.enable();

    // Buffer size = exactly one chunk's worth of I/Q samples. Sizing
    // it to match means push() ships only modulated audio with no
    // trailing zero-pad — anything else creates audible gaps because
    // the AD9361 DAC clocks every sample in the buffer regardless of
    // whether it carries signal.
    let chunk_iq_samples = TX_CHUNK_AUDIO_SAMPLES * session.negotiated_rate.ratio;
    let buf = session
        .tx_buffer_dev
        .create_buffer(chunk_iq_samples, false)
        .map_err(|e| PlutoError::Stream(format!("create TX buffer: {e}")))?;

    // DSP chain — built once, reused on every chunk.
    let if_rate = session.negotiated_rate.sample_rate_hz as f32;
    let ratio = session.negotiated_rate.ratio;
    // The interpolator's prototype taps: same windowed-sinc design as
    // the matching decimator. 99 taps is plenty for 24 kHz cutoff
    // anti-imaging in front of the AD9361's HB chain; the AD9361's
    // own 4× decim/interp FIR plus internal HB filters do the rest.
    let interp_taps = modem_sdr_dsp::decimator::PolyphaseDecimator::hamming_sinc_taps(
        if_rate,
        modem_sdr_dsp::AUDIO_RATE as f32 / 2.0,
        99,
    );
    let mut interp = PolyphaseInterpolator::with_taps(interp_taps, ratio);
    let pm = PhaseMod::calibrated();

    let mut last_tick = std::time::Instant::now();
    let mut total_iq_pushed: u64 = 0;
    let mut push_errors: u64 = 0;

    // Pre-allocate the I/Q chunk packing buffers — one per push, sized
    // to match the libiio buffer exactly (chunk_iq_samples each).
    let mut i_buf: Vec<i16> = vec![0; chunk_iq_samples];
    let mut q_buf: Vec<i16> = vec![0; chunk_iq_samples];

    let mut audio_idx = 0usize;
    while audio_idx < audio.len() && !stop.load(Ordering::Relaxed) {
        let end = (audio_idx + TX_CHUNK_AUDIO_SAMPLES).min(audio.len());
        let mut chunk: Vec<f32> = audio[audio_idx..end].to_vec();
        // For a short final chunk, pad the AUDIO side with zeros so
        // the interpolator + PM still produce a full chunk_iq_samples
        // buffer. The trailing zero-audio becomes a constant-phase
        // carrier — silent on the receive side, no audible artifact.
        if chunk.len() < TX_CHUNK_AUDIO_SAMPLES {
            chunk.resize(TX_CHUNK_AUDIO_SAMPLES, 0.0);
        }

        // Audio → IF rate via polyphase interpolation.
        let if_audio = interp.process(&chunk);
        debug_assert_eq!(if_audio.len(), chunk_iq_samples);

        // Real audio → Complex<f32> via PhaseMod.
        let mut iq = vec![Complex32::new(0.0, 0.0); if_audio.len()];
        pm.process(&if_audio, &mut iq);

        // Pack to S16 i16 — peak ±32_000 leaves headroom under the
        // AD9361 DAC ceiling.
        for (k, c) in iq.iter().enumerate() {
            i_buf[k] = (c.re * TX_S16_SCALE).round() as i16;
            q_buf[k] = (c.im * TX_S16_SCALE).round() as i16;
        }

        if let Err(e) = i_chan.write::<i16>(&buf, &i_buf) {
            return Err(PlutoError::Stream(format!("TX write I: {e}")));
        }
        if let Err(e) = q_chan.write::<i16>(&buf, &q_buf) {
            return Err(PlutoError::Stream(format!("TX write Q: {e}")));
        }
        match buf.push() {
            Ok(_n) => {
                total_iq_pushed += iq.len() as u64;
            }
            Err(e) => {
                push_errors += 1;
                if last_tick.elapsed() >= STATUS_TICK_PERIOD {
                    eprintln!("[pluto-tx] push error #{push_errors}: {e}");
                    last_tick = std::time::Instant::now();
                }
            }
        }

        audio_idx = end;
        pos.store(audio_idx, Ordering::Relaxed);

        if last_tick.elapsed() >= STATUS_TICK_PERIOD {
            eprintln!(
                "[pluto-tx] tick: {audio_idx}/{} audio samples submitted, \
                 {total_iq_pushed} I/Q pushed, {push_errors} push errors",
                audio.len()
            );
            last_tick = std::time::Instant::now();
        }
    }

    Ok(())
}
