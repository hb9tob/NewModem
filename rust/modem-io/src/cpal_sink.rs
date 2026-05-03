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
use cpal::{SampleFormat, SampleRate};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
        let format = range.sample_format();
        let cfg = range.with_sample_rate(SampleRate(sample_rate));
        let channels = cfg.channels() as usize;
        let stream_cfg: cpal::StreamConfig = cfg.into();

        let total_samples = samples.len();
        let pos = Arc::new(AtomicUsize::new(0));
        let pos_cb = pos.clone();
        let err_cb = |e| eprintln!("[tx] stream err: {e}");

        let samples_arc: Arc<[f32]> = samples.into();
        let s_f32 = samples_arc.clone();
        let s_i16 = samples_arc.clone();
        let s_u16 = samples_arc.clone();

        let stream = match format {
            SampleFormat::F32 => device
                .build_output_stream::<f32, _, _>(
                    &stream_cfg,
                    move |data, _| write_out_f32(data, channels, &s_f32, &pos_cb),
                    err_cb,
                    None,
                )
                .map_err(|e| IoError::Backend(format!("build_output_stream: {e}")))?,
            SampleFormat::I16 => device
                .build_output_stream::<i16, _, _>(
                    &stream_cfg,
                    move |data, _| write_out_i16(data, channels, &s_i16, &pos_cb),
                    err_cb,
                    None,
                )
                .map_err(|e| IoError::Backend(format!("build_output_stream: {e}")))?,
            SampleFormat::U16 => device
                .build_output_stream::<u16, _, _>(
                    &stream_cfg,
                    move |data, _| write_out_u16(data, channels, &s_u16, &pos_cb),
                    err_cb,
                    None,
                )
                .map_err(|e| IoError::Backend(format!("build_output_stream: {e}")))?,
            other => return Err(IoError::UnsupportedFormat(format!("{other:?}"))),
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
