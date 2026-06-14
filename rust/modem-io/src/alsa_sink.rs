//! Direct-ALSA `SampleSink` for sound-card TX output (Linux only).
//!
//! The cpal sink (`cpal_sink`) routes playback through cpal's ALSA host,
//! which can land on `plug`/`dmix`/`default` and silently resample — the
//! libspeexdsp fixed-point overflow bug distorts loud audio, fatal for an
//! APSK data modem whose information lives in the amplitude rings. This
//! sink opens the card's PCM **directly as `hw:`** at the card's native
//! S16_LE / 48 kHz, so the only processing between our `Vec<f32>` and the
//! wire is the per-sample clip + mono → N-channel duplication, identical
//! to what `cpal_sink::write_out_i16` produced on the OTA-validated path.
//!
//! (The codec's "Auto Gain Control" still lives below this — that is
//! handled separately by `alsa_mixer::disable_agc`, called by the TX
//! worker before each burst.)

use crate::alsa_pcm::{self, TARGET_RATE};
use crate::traits::{IoError, PlaybackHandle, SampleSink};
use alsa::pcm::PCM;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Output period / kernel-buffer depth. Same rationale as the cpal sink's
/// `TX_BUFFER_MS = 100`: a large period gives the OS audio path slack to
/// survive a WebKit scheduling stall on a Pi without an underrun (audible
/// click / dropout on air). 4 periods = 400 ms of buffered tail.
const PERIOD_MS: u32 = 100;
const BUFFER_PERIODS: u32 = 4;

#[derive(Default, Clone, Copy)]
pub struct AlsaSink;

impl SampleSink for AlsaSink {
    fn play_buffer(
        &self,
        device_name: &str,
        sample_rate: u32,
        samples: Vec<f32>,
    ) -> Result<PlaybackHandle, IoError> {
        if sample_rate != TARGET_RATE {
            return Err(IoError::UnsupportedSampleRate {
                device: device_name.to_string(),
                rate: sample_rate,
            });
        }
        let pcm_name = alsa_pcm::hw_pcm_name(device_name)
            .ok_or_else(|| IoError::DeviceNotFound(device_name.to_string()))?;
        let pcm = PCM::new(&pcm_name, alsa::Direction::Playback, false)
            .map_err(|e| IoError::Backend(format!("open playback '{pcm_name}': {e}")))?;
        let (channels, period_frames) = alsa_pcm::configure(&pcm, PERIOD_MS, BUFFER_PERIODS)
            .map_err(|e| IoError::Backend(format!("configure '{pcm_name}': {e}")))?;
        pcm.prepare()
            .map_err(|e| IoError::Backend(format!("prepare '{pcm_name}': {e}")))?;

        alsa_pcm::log(&format!(
            "[alsa-tx] '{pcm_name}' opened: {channels}ch S16_LE {TARGET_RATE}Hz period={period_frames}f (~{PERIOD_MS}ms), {} samples to play",
            samples.len()
        ));

        let total_samples = samples.len();
        let pos = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let pos_w = pos.clone();
        let stop_w = stop.clone();
        let join = thread::spawn(move || {
            run_writer(pcm, channels, period_frames, samples, pos_w, stop_w);
        });

        // The guard owns the writer thread; dropping the PlaybackHandle
        // (caller stops early) signals stop + joins, killing audio.
        let guard = AlsaPlayGuard {
            stop,
            join: Some(join),
        };
        Ok(PlaybackHandle::new(pos, total_samples, Box::new(guard)))
    }
}

/// Writer thread: clip f32 → i16, duplicate mono across `channels`, push
/// period-sized blocks with blocking `writei`, and keep `pos` updated with
/// frames *actually played* (submitted − still-queued) so the TX worker's
/// PTT-release poll lands on the real end-of-air.
fn run_writer(
    pcm: PCM,
    channels: u16,
    period_frames: usize,
    samples: Vec<f32>,
    pos: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) {
    let io = match pcm.io_i16() {
        Ok(io) => io,
        Err(e) => {
            alsa_pcm::log(&format!("[alsa-tx] io_i16 failed: {e}"));
            return;
        }
    };
    let ch = channels as usize;
    let total = samples.len();
    let mut frame_buf = vec![0i16; period_frames * ch];
    let mut idx = 0usize;

    while idx < total && !stop.load(Ordering::Relaxed) {
        let n = (total - idx).min(period_frames);
        for i in 0..n {
            // Same quantization as cpal_sink::write_out_i16 — bit-for-bit
            // identical samples reach the card.
            let v = (samples[idx + i] * 32767.0).clamp(-32768.0, 32767.0) as i16;
            for c in 0..ch {
                frame_buf[i * ch + c] = v;
            }
        }
        match io.writei(&frame_buf[..n * ch]) {
            Ok(written) => {
                idx += written;
            }
            Err(e) => {
                // EPIPE = underrun (we fell behind). Recover and retry the
                // same block; a hard failure ends the burst.
                if let Err(e2) = pcm.try_recover(e, true) {
                    alsa_pcm::log(&format!("[alsa-tx] unrecoverable write error: {e2}"));
                    break;
                }
                let _ = pcm.prepare();
            }
        }
        // Frames still queued in the kernel buffer → played = submitted − queued.
        let queued = pcm.delay().unwrap_or(0).max(0) as usize;
        pos.store(idx.saturating_sub(queued).min(total), Ordering::Relaxed);
    }

    if stop.load(Ordering::Relaxed) {
        // Early stop: don't drain the queued tail; closing the PCM at
        // scope end (io drops first, then PCM's snd_pcm_close) stops audio
        // promptly without playing out the buffered remainder.
        alsa_pcm::log("[alsa-tx] stop flag → discarding tail");
        return;
    }
    // Normal end: block until the buffered tail has actually played out,
    // then mark complete so the worker releases PTT only after end-of-air.
    let _ = pcm.drain();
    pos.store(total, Ordering::Relaxed);
    alsa_pcm::log("[alsa-tx] burst drained — playback complete");
}

/// Keeps the writer thread alive with the `PlaybackHandle`. Drop = stop
/// the burst and join the thread (mirrors cpal's "drop the Stream stops
/// audio" contract).
struct AlsaPlayGuard {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for AlsaPlayGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
