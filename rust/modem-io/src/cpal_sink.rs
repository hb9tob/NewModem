//! cpal-backed `SampleSink` for sound-card output.
//!
//! Extracted verbatim from `modem-worker::tx_worker::run_playback`
//! (phase 3b of the layered-arch refactor). The format selection (F32 →
//! I16 → U16 preference), the mono → N-channel duplication and the
//! per-format clipping are all preserved bit-for-bit so the audio that
//! reaches the sound card is identical to what the OTA-validated
//! pipeline produced before the extraction.

use crate::traits::{IoError, PlaybackHandle, SampleSink};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, SupportedBufferSize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Target ALSA/WASAPI output period, in ms. Mirrors the capture side's
/// `TARGET_BUFFER_MS`: the default period the backend negotiates can be
/// as small as ~10 ms on a Pi 4 USB sound card, which is too short to
/// survive a scheduling stall when the WebKit GUI hogs a core — the
/// memcpy callback misses its deadline and ALSA underruns (audible click
/// / dropout on air). A 100 ms buffer gives the OS audio thread far more
/// slack at the cost of ~100 ms extra latency, irrelevant for a one-shot
/// pre-rendered TX burst. Clamped to the device-reported range; falls
/// back to `Default` if the device refuses the fixed size.
const TX_BUFFER_MS: u32 = 100;

#[derive(Default, Clone, Copy)]
pub struct CpalSink;

impl SampleSink for CpalSink {
    fn play_buffer(
        &self,
        device_name: &str,
        sample_rate: u32,
        samples: Vec<f32>,
    ) -> Result<PlaybackHandle, IoError> {
        let host = cpal::default_host();
        let device = host
            .output_devices()
            .map_err(|e| IoError::Backend(format!("output_devices: {e}")))?
            .find(|d| d.name().map(|n| n == device_name).unwrap_or(false))
            .ok_or_else(|| IoError::DeviceNotFound(device_name.to_string()))?;

        let configs = device
            .supported_output_configs()
            .map_err(|e| IoError::Backend(format!("supported_output_configs: {e}")))?
            .collect::<Vec<_>>();
        let supports_rate: Vec<_> = configs
            .into_iter()
            .filter(|c| c.min_sample_rate().0 <= sample_rate && sample_rate <= c.max_sample_rate().0)
            .collect();
        if supports_rate.is_empty() {
            return Err(IoError::UnsupportedSampleRate {
                device: device_name.to_string(),
                rate: sample_rate,
            });
        }
        fn rank(f: SampleFormat) -> u8 {
            match f {
                SampleFormat::F32 => 0,
                SampleFormat::I16 => 1,
                SampleFormat::U16 => 2,
                _ => 4,
            }
        }
        let range = supports_rate
            .into_iter()
            .min_by_key(|c| rank(c.sample_format()))
            .unwrap();
        let supported_buf = *range.buffer_size();
        let format = range.sample_format();
        let cfg = range.with_sample_rate(SampleRate(sample_rate));
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.into();

        // Pick an output period in line with `TX_BUFFER_MS`, clamped to the
        // device-reported range. `Unknown` falls through to `Default` (the
        // historical behaviour). See the `TX_BUFFER_MS` doc-comment for why
        // the default is too small on a Pi 4.
        let target_frames = sample_rate * TX_BUFFER_MS / 1000;
        let chosen_buf = match supported_buf {
            SupportedBufferSize::Range { min, max } => {
                BufferSize::Fixed(target_frames.clamp(min, max))
            }
            SupportedBufferSize::Unknown => BufferSize::Default,
        };

        let total_samples = samples.len();
        let pos = Arc::new(AtomicUsize::new(0));
        let err_cb = |e| eprintln!("[tx] stream err: {e}");

        let samples_arc: Arc<[f32]> = samples.into();

        // Build the output stream for `buf_size`. Cloned cheaply (Arc
        // refcount + atomic) so the `Fixed → Default` fallback can retry
        // from a clean slate.
        let make_stream = |buf_size: BufferSize| {
            let mut local_cfg = stream_cfg.clone();
            local_cfg.buffer_size = buf_size;
            let pos_cb = pos.clone();
            match format {
                SampleFormat::F32 => {
                    let s = samples_arc.clone();
                    device.build_output_stream::<f32, _, _>(
                        &local_cfg,
                        move |data, _| write_out_f32(data, channels, &s, &pos_cb),
                        err_cb,
                        None,
                    )
                }
                SampleFormat::I16 => {
                    let s = samples_arc.clone();
                    device.build_output_stream::<i16, _, _>(
                        &local_cfg,
                        move |data, _| write_out_i16(data, channels, &s, &pos_cb),
                        err_cb,
                        None,
                    )
                }
                SampleFormat::U16 => {
                    let s = samples_arc.clone();
                    device.build_output_stream::<u16, _, _>(
                        &local_cfg,
                        move |data, _| write_out_u16(data, channels, &s, &pos_cb),
                        err_cb,
                        None,
                    )
                }
                other => {
                    return Err(IoError::UnsupportedFormat(format!("{other:?}")));
                }
            }
            .map_err(|e| IoError::Backend(format!("build_output_stream: {e}")))
        };

        // First attempt: the configured `Fixed(N)`. On refusal, revert to
        // `Default` (the historical small-buffer behaviour) rather than
        // failing the whole TX.
        let stream = match make_stream(chosen_buf) {
            Ok(s) => s,
            Err(e) if matches!(chosen_buf, BufferSize::Fixed(_)) => {
                eprintln!("[tx] {chosen_buf:?} refused ({e}), falling back to BufferSize::Default");
                make_stream(BufferSize::Default)?
            }
            Err(e) => return Err(e),
        };

        stream
            .play()
            .map_err(|e| IoError::Backend(format!("stream.play: {e}")))?;

        Ok(PlaybackHandle::new(pos, total_samples, Box::new(stream)))
    }
}

fn write_out_f32(out: &mut [f32], channels: usize, samples: &[f32], pos: &AtomicUsize) {
    let frames = out.len() / channels;
    let p = pos.load(Ordering::Relaxed);
    let avail = samples.len().saturating_sub(p);
    let n = frames.min(avail);
    for i in 0..n {
        let v = samples[p + i];
        for c in 0..channels {
            out[i * channels + c] = v;
        }
    }
    for i in n..frames {
        for c in 0..channels {
            out[i * channels + c] = 0.0;
        }
    }
    pos.fetch_add(n, Ordering::Relaxed);
}

fn write_out_i16(out: &mut [i16], channels: usize, samples: &[f32], pos: &AtomicUsize) {
    let frames = out.len() / channels;
    let p = pos.load(Ordering::Relaxed);
    let avail = samples.len().saturating_sub(p);
    let n = frames.min(avail);
    for i in 0..n {
        let v = (samples[p + i] * 32767.0).clamp(-32768.0, 32767.0) as i16;
        for c in 0..channels {
            out[i * channels + c] = v;
        }
    }
    for i in n..frames {
        for c in 0..channels {
            out[i * channels + c] = 0;
        }
    }
    pos.fetch_add(n, Ordering::Relaxed);
}

fn write_out_u16(out: &mut [u16], channels: usize, samples: &[f32], pos: &AtomicUsize) {
    let frames = out.len() / channels;
    let p = pos.load(Ordering::Relaxed);
    let avail = samples.len().saturating_sub(p);
    let n = frames.min(avail);
    for i in 0..n {
        let v = ((samples[p + i] * 32767.0).clamp(-32768.0, 32767.0) as i32 + 32768) as u16;
        for c in 0..channels {
            out[i * channels + c] = v;
        }
    }
    for i in n..frames {
        for c in 0..channels {
            out[i * channels + c] = 32768;
        }
    }
    pos.fetch_add(n, Ordering::Relaxed);
}
