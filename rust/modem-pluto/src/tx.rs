//! Pluto TX path: 48 kHz mono audio `Vec<f32>` → iiod-TCP I/Q at 576 / 2304 kSa/s.
//!
//! The contract mirrors `modem_io::traits::SampleSink` (one-shot full
//! buffer submit, returns a progress handle). The TX worker owns its
//! own [`IiodClient`] (one TCP connection per stream — cleanly
//! independent of any concurrent RX worker), runs the radio-faithful
//! upsample + PM chain, packs the result to S16 I/Q, and pushes it
//! into the AD9361 over WRITEBUF.
//!
//! Chain (matches `modem_sdr_dsp`'s integration test layout):
//!
//! ```text
//! Vec<f32> @ 48 kHz audio
//!   → PolyphaseInterpolator (×N to AD9361 IF rate)
//!   → PhaseMod (radio-faithful PM, +6 dB/oct preemph for free)
//!   → pack to S16/16 LE interleaved (peak ±32000 — leaves headroom
//!     under the AD9361 DAC ceiling so PM peaks don't clip)
//!   → iiod WRITEBUF on cf-ad9361-dds-core-lpc
//! ```
//!
//! ## Why chunked streaming and not one big upload
//!
//! Worst-case math: 5 minutes of audio at 48 kHz = 14.4 M f32 samples
//! (57.6 MB). Interpolated to 576 kHz Complex<f32> that's 173 M
//! samples (1.4 GB). Packed S16 I/Q is 691 MB. We can't allocate that
//! up front. The TX worker loops over the input audio in
//! [`TX_CHUNK_AUDIO_SAMPLES`]-sized batches, runs each batch through
//! the chain, packs it to S16 I/Q, and pushes it via `write_buffer`.
//! WRITEBUF blocks until the previous batch has been clocked out
//! kernel-side, which gives natural backpressure against the DAC.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use num_complex::Complex32;

use modem_sdr_dsp::ctcss_gen::CtcssToneGen;
use modem_sdr_dsp::interpolator::PolyphaseInterpolator;
use modem_sdr_dsp::pm_mod::PhaseMod;

use crate::device::{self, PlutoConfig, PlutoSession};
use crate::error::PlutoError;
use crate::iiod::IiodClient;

/// Audio batch size pulled from the input `Vec<f32>` per chain run.
/// 4096 samples = ~85 ms of audio at 48 kHz. The IIO TX buffer is
/// sized to hold exactly `TX_CHUNK_AUDIO_SAMPLES * ratio` I/Q samples
/// so each WRITEBUF sends only modulated audio — no zero-padding,
/// which would otherwise inject silent gaps between chunks (the chip
/// clocks every sample in the buffer to its DAC, padding included).
const TX_CHUNK_AUDIO_SAMPLES: usize = 4096;

/// AD9361 / AD9363 TX path on Pluto consumes S16/16 LE — full int16
/// range. We scale unit-amplitude `Complex32` PM output by this
/// constant. PhaseMod is constant-envelope (|y| = 1) so peak input
/// is exactly 1.0; we leave a safety margin below `i16::MAX` so any
/// quantization noise doesn't clip.
const TX_S16_SCALE: f32 = 32_000.0;

/// Bytes per scan cycle on Pluto TX: I (i16 LE) + Q (i16 LE).
const BYTES_PER_SCAN: usize = 4;

/// Channel-enable mask for `cf-ad9361-dds-core-lpc`. Bit 0 = voltage0
/// (I), bit 1 = voltage1 (Q). Both on for normal complex TX.
const TX_CHANNEL_MASK: u32 = 0x0000_0003;

/// Throttled-error tick.
const STATUS_TICK_PERIOD: Duration = Duration::from_secs(2);

/// `SampleSink`-shaped TX target for Pluto.
///
/// Cheap to clone (just config). The actual iiod connection is opened
/// per-job in [`Self::play_buffer`]; clones don't share any
/// connection state, so the caller can keep one for retune-ish UI
/// concerns while the worker thread runs another.
#[derive(Clone, Debug)]
pub struct PlutoSink {
    config: PlutoConfig,
}

impl PlutoSink {
    pub fn new(config: PlutoConfig) -> Self {
        Self { config }
    }

    /// One-shot full-buffer submit. Spawns a worker thread that
    /// programs the AD9361 via [`device::open`] (transient control
    /// connection), opens its own streaming iiod connection, runs the
    /// upsample + PM chain on `audio_samples` in chunks, and pushes
    /// the resulting I/Q via WRITEBUF on `cf-ad9361-dds-core-lpc`.
    /// Returns a [`TxJob`] the caller polls / drops.
    ///
    /// Returns immediately after the AD9361 has been programmed and
    /// the streaming buffer opened, matching `cpal_capture::start`'s
    /// ready-handshake pattern.
    pub fn play_buffer(&self, audio_samples: Vec<f32>) -> Result<TxJob, PlutoError> {
        let total_samples = audio_samples.len();
        let pos = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), PlutoError>>();

        let pos_thread = pos.clone();
        let stop_thread = stop.clone();
        let cfg = self.config.clone();

        let thread = thread::spawn(move || {
            let _ = run_tx(cfg, audio_samples, pos_thread, stop_thread, ready_tx);
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

    /// Run TX on an already-configured session — used by the loopback
    /// test where one Pluto serves both directions. Unlike
    /// [`Self::play_buffer`], this is **synchronous**: it returns
    /// after the entire `audio_samples` buffer has been pushed and
    /// drained. Caller spawns its own thread if it wants async.
    pub fn play_on(
        session: PlutoSession,
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
    session: PlutoSession,
    audio: &[f32],
    pos: &AtomicUsize,
    stop: Arc<AtomicBool>,
) -> Result<(), PlutoError> {
    // Open a dedicated TX streaming connection (independent of any
    // concurrent RX, independent of the control connection that
    // `device::open` opened-and-dropped).
    let mut client = IiodClient::connect(&session.config.uri)?;
    let _ = client.set_iiod_timeout(2000);

    // Buffer size = exactly one chunk's worth of I/Q samples. Sizing
    // it to match means each WRITEBUF ships only modulated audio with
    // no trailing zero-pad — anything else creates audible gaps
    // because the AD9361 DAC clocks every sample in the buffer
    // regardless of whether it carries signal.
    let chunk_iq_samples = TX_CHUNK_AUDIO_SAMPLES * session.negotiated_rate.ratio;
    client
        .open_buffer(
            crate::device::iio_names::TX_BUFFER,
            chunk_iq_samples,
            TX_CHANNEL_MASK,
            false,
        )
        .map_err(|e| PlutoError::Stream(format!("OPEN cf-ad9361-dds-core-lpc: {e}")))?;

    // DSP chain — built once, reused on every chunk.
    let if_rate = session.negotiated_rate.sample_rate_hz as f32;
    let ratio = session.negotiated_rate.ratio;
    // The interpolator's prototype taps: same windowed-sinc design as
    // the matching decimator. 99 taps is plenty for 24 kHz cutoff
    // anti-imaging in front of the AD9361's HB chain.
    let interp_taps = modem_sdr_dsp::decimator::PolyphaseDecimator::hamming_sinc_taps(
        if_rate,
        modem_sdr_dsp::AUDIO_RATE as f32 / 2.0,
        99,
    );
    let mut interp = PolyphaseInterpolator::with_taps(interp_taps, ratio);
    // Scale `k_p` linearly from the 5 kHz calibration so audio at
    // unit amplitude produces ±`session.tx_deviation_hz` on the air.
    let k_p = (session.tx_deviation_hz / 5000.0) * PhaseMod::DEFAULT_K_P;
    let pm = PhaseMod::new(k_p);

    // CTCSS sub-audible tone for repeater squelch. Built only if
    // configured; the audio loop calls `process_add` on each chunk so
    // the tone is summed into the voice *before* the polyphase
    // interpolator and PhaseMod — i.e. it goes through the FM
    // modulator like any other audio component.
    let mut ctcss = if session.ctcss_freq_hz > 0.0 {
        Some(CtcssToneGen::new(
            session.ctcss_freq_hz,
            modem_sdr_dsp::AUDIO_RATE as f32,
            session.ctcss_level,
        ))
    } else {
        None
    };

    let mut last_tick = std::time::Instant::now();
    let mut total_iq_pushed: u64 = 0;
    let mut push_errors: u64 = 0;

    // Pre-allocated wire-format scratch — interleaved I/Q i16 LE.
    // Reused across every WRITEBUF call.
    let buffer_bytes = chunk_iq_samples * BYTES_PER_SCAN;
    let mut wire_buf: Vec<u8> = vec![0u8; buffer_bytes];

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

        // Mix in the CTCSS sub-audible tone *before* interpolation.
        if let Some(ref mut t) = ctcss {
            t.process_add(&mut chunk);
        }

        // Audio → IF rate via polyphase interpolation.
        let if_audio = interp.process(&chunk);
        debug_assert_eq!(if_audio.len(), chunk_iq_samples);

        // Real audio → Complex<f32> via PhaseMod.
        let mut iq = vec![Complex32::new(0.0, 0.0); if_audio.len()];
        pm.process(&if_audio, &mut iq);

        // Pack to interleaved S16 LE I/Q. AD9361 TX wire format is
        // I_lo, I_hi, Q_lo, Q_hi, repeat — same byte layout the
        // libiio FFI path used to write via channel::write::<i16>.
        for (k, c) in iq.iter().enumerate() {
            let i = (c.re * TX_S16_SCALE).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            let q = (c.im * TX_S16_SCALE).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            let off = k * BYTES_PER_SCAN;
            wire_buf[off..off + 2].copy_from_slice(&i.to_le_bytes());
            wire_buf[off + 2..off + 4].copy_from_slice(&q.to_le_bytes());
        }

        match client.write_buffer(crate::device::iio_names::TX_BUFFER, &wire_buf) {
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

    // Best-effort cleanup.
    let _ = client.close_buffer(crate::device::iio_names::TX_BUFFER);
    let _ = client.close();
    Ok(())
}
