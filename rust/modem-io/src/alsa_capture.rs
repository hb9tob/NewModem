//! Direct-ALSA capture → 48 kHz mono f32 sample channel (Linux only).
//!
//! The cpal capture path (`cpal_capture`) goes through cpal's ALSA host,
//! which on a Pi can be routed via `plug`/`dmix`/`default` and silently
//! resample (libspeexdsp, fixed-point overflow bug — fatal for a data
//! modem). This backend opens the card's PCM **directly as `hw:`** so the
//! kernel hands us the card's native S16_LE stream untouched: no
//! resampling, no dmix mixing, no softvol.
//!
//! Public surface mirrors [`crate::cpal_capture::start`] exactly so the
//! GUI/worker call sites are backend-agnostic: same `CaptureHandle`, same
//! `Receiver<Vec<f32>>` carrying 48 kHz mono f32 (channel 0 of each
//! frame, matching the cpal decoder), same `dropped_samples` semantics
//! (incremented on an ALSA overrun = the reader thread fell behind, which
//! the rx_worker reads to brickwall → flush → idle).
//!
//! Architecture is simpler than cpal's three-thread ring dance: a single
//! blocking-read thread owns the PCM and drains it period-by-period. ALSA
//! itself provides the kernel-side ring; a generous period (~100 ms) gives
//! the same scheduling slack the cpal 30 s ring + 100 ms buffer bought us.

use crate::cpal_capture::CaptureHandle;
use std::sync::mpsc::{self, Receiver};

#[cfg(target_os = "linux")]
pub fn start(device_name: &str) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
    imp::start(device_name)
}

#[cfg(not(target_os = "linux"))]
pub fn start(_device_name: &str) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
    Err("direct ALSA capture is only available on Linux".into())
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use crate::alsa_pcm::{self, TARGET_RATE};
    use alsa::pcm::{State, IO, PCM};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Target capture period. Mirrors the cpal side's 100 ms buffer: a
    /// large period means the kernel only needs to wake our reader ~10×/s,
    /// surviving a WebKit scheduling stall on a Pi without an overrun.
    const PERIOD_MS: u32 = 100;
    /// Kernel ring depth, in periods. 4 × 100 ms = 400 ms of slack before
    /// an overrun is even possible.
    const BUFFER_PERIODS: u32 = 4;

    pub fn start(device_name: &str) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
        let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
        let stop = Arc::new(AtomicBool::new(false));
        let dropped_samples = Arc::new(AtomicU64::new(0));

        // Open + negotiate on this thread so device-not-found / format
        // errors surface synchronously to the caller (like cpal's
        // ready_tx phase). The PCM is `Send`, so we hand it to the reader.
        let (pcm, channels, period_frames) = open_capture(device_name)?;

        let stop_reader = stop.clone();
        let dropped_reader = dropped_samples.clone();
        let reader = thread::spawn(move || {
            run_reader(
                pcm,
                channels,
                period_frames,
                sample_tx,
                dropped_reader,
                stop_reader,
            );
        });

        Ok((
            CaptureHandle {
                stop,
                threads: Some(vec![reader]),
                sample_rate: TARGET_RATE,
                channels,
                dropped_samples,
            },
            sample_rx,
        ))
    }

    /// Open `device_name` as a direct `hw:` capture PCM at 48 kHz S16_LE,
    /// returning the configured PCM, its channel count and the negotiated
    /// period size (frames). Errors carry the ALSA failure verbatim.
    fn open_capture(device_name: &str) -> Result<(PCM, u16, usize), String> {
        let pcm_name = alsa_pcm::hw_pcm_name(device_name)
            .ok_or_else(|| format!("'{device_name}' is not a direct ALSA hw: card"))?;
        // blocking = false → second arg `false` opens in blocking mode.
        let pcm = PCM::new(&pcm_name, alsa::Direction::Capture, false)
            .map_err(|e| format!("open capture '{pcm_name}': {e}"))?;

        let (channels, period_frames) =
            alsa_pcm::configure(&pcm, PERIOD_MS, BUFFER_PERIODS).map_err(|e| {
                format!("configure capture '{pcm_name}': {e}")
            })?;

        pcm.prepare()
            .map_err(|e| format!("prepare capture '{pcm_name}': {e}"))?;

        crate::alsa_pcm::log(&format!(
            "[alsa-cap] '{pcm_name}' opened: {channels}ch S16_LE {TARGET_RATE}Hz period={period_frames}f (~{PERIOD_MS}ms) buffer={BUFFER_PERIODS}×period"
        ));
        Ok((pcm, channels, period_frames))
    }

    fn run_reader(
        pcm: PCM,
        channels: u16,
        period_frames: usize,
        sample_tx: mpsc::Sender<Vec<f32>>,
        dropped_samples: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
    ) {
        let ch = channels as usize;
        // Interleaved i16 scratch: one period of `channels` samples.
        let mut buf = vec![0i16; period_frames * ch];
        let io: IO<i16> = match pcm.io_i16() {
            Ok(io) => io,
            Err(e) => {
                crate::alsa_pcm::log(&format!("[alsa-cap] io_i16 failed: {e}"));
                return;
            }
        };

        let mut last_tick = Instant::now();
        let mut frames_total: u64 = 0;
        let mut overruns: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            let frames = match io.readi(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    // EPIPE = overrun (reader fell behind). Recover and
                    // signal the lost period to the worker via the same
                    // mono-sample counter cpal uses for its brickwall.
                    overruns += 1;
                    dropped_samples.fetch_add(period_frames as u64, Ordering::Relaxed);
                    if let Err(e2) = pcm.try_recover(e, true) {
                        crate::alsa_pcm::log(&format!(
                            "[alsa-cap] unrecoverable read error: {e2} — exiting reader"
                        ));
                        break;
                    }
                    // After recovery the PCM is back in Prepared; the next
                    // readi auto-starts it.
                    if pcm.state() == State::Prepared {
                        let _ = pcm.start();
                    }
                    continue;
                }
            };
            if frames == 0 {
                continue;
            }
            frames_total += frames as u64;

            // Channel 0 → f32, matching cpal_capture's mono decoder
            // (`decode_native_to_mono_f32` keeps the first channel).
            let mut mono = Vec::with_capacity(frames);
            for f in 0..frames {
                mono.push(buf[f * ch] as f32 / 32768.0);
            }
            // Unbounded mpsc: never blocks the reader. Worker gone → Err,
            // ignored; the stop flag ends the loop next iteration.
            if sample_tx.send(mono).is_err() {
                break;
            }

            let now = Instant::now();
            if now.duration_since(last_tick) >= Duration::from_secs(2) {
                let dt = now.duration_since(last_tick).as_secs_f64();
                crate::alsa_pcm::log(&format!(
                    "[alsa-cap] tick: {rate:.0} Sa/s ≈ {ratio:.2}× target, overruns={overruns}, dropped={dropped} mono-samples",
                    rate = frames_total as f64 / dt,
                    ratio = frames_total as f64 / (TARGET_RATE as f64 * dt),
                    dropped = dropped_samples.load(Ordering::Relaxed),
                ));
                last_tick = now;
                frames_total = 0;
            }
        }
        // `io` borrows `pcm`; both close when this scope ends (io first,
        // then the PCM's Drop runs snd_pcm_close).
        crate::alsa_pcm::log("[alsa-cap] stop flag → closing PCM");
    }
}
