//! Continuous audio-monitor output for the Radio tab.
//!
//! Unlike [`crate::cpal_sink::CpalSink`] — which plays one pre-rendered
//! TX buffer to completion — the monitor is a long-lived output stream
//! the RX path feeds in real time: the worker `push()`es each decoded
//! 48 kHz mono chunk and the cpal callback drains it to the chosen
//! sound card, applying an operator-set volume. Underruns play silence
//! (a brief gap) rather than blocking the capture thread.
//!
//! Threading: cpal's `Stream` is `!Send` on Windows, so [`AudioMonitor`]
//! must be owned by the thread that built it (the RX worker thread).
//! Only [`AudioMonitor::push`] and [`AudioMonitor::set_volume`] are
//! touched from elsewhere, and both go through `Arc`-shared state
//! (a mutex-guarded ring + an atomic gain) that is `Send + Sync`.
//! Device changes ([`AudioMonitor::set_device`]) rebuild the stream and
//! so must run on the owning thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, SupportedBufferSize};

use crate::traits::IoError;

/// Maximum audio buffered ahead of the sound card, in ms. Caps added
/// latency and bounds memory if the device drains slower than the RX
/// produces. Older samples are dropped once exceeded.
const MAX_RING_MS: u32 = 250;

/// Target output period, in ms. Smaller than the TX sink's 100 ms for
/// snappier live monitoring while still surviving GUI scheduling jitter.
const MONITOR_BUFFER_MS: u32 = 60;

/// State shared with the cpal callback.
struct Shared {
    /// Mono 48 kHz samples awaiting playback.
    ring: Mutex<VecDeque<f32>>,
    /// Playback gain, stored as `f32` bits. 1.0 = unity.
    volume_bits: AtomicU32,
    /// Cap on `ring` length (samples).
    cap_samples: usize,
}

impl Shared {
    fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }
}

/// Live audio-monitor output stream. Drop = stop.
pub struct AudioMonitor {
    /// Kept alive for the duration of monitoring; dropping stops audio.
    stream: cpal::Stream,
    shared: Arc<Shared>,
    sample_rate: u32,
    device_name: String,
}

impl AudioMonitor {
    /// Open `device_name` at `sample_rate` Hz mono-source and start a
    /// silent output stream ready to be `push()`ed into.
    pub fn start(device_name: &str, sample_rate: u32) -> Result<Self, IoError> {
        let cap_samples = (sample_rate * MAX_RING_MS / 1000) as usize;
        let shared = Arc::new(Shared {
            ring: Mutex::new(VecDeque::with_capacity(cap_samples)),
            volume_bits: AtomicU32::new(1.0_f32.to_bits()),
            cap_samples,
        });
        let stream = build_stream(device_name, sample_rate, &shared)?;
        stream
            .play()
            .map_err(|e| IoError::Backend(format!("monitor stream.play: {e}")))?;
        Ok(Self {
            stream,
            shared,
            sample_rate,
            device_name: device_name.to_string(),
        })
    }

    /// Queue decoded mono samples for playback. Drops the oldest audio
    /// if the ring would exceed [`MAX_RING_MS`] — better a small gap
    /// than ever-growing latency. Never blocks the producer beyond the
    /// brief mutex hold.
    pub fn push(&self, samples: &[f32]) {
        if let Ok(mut ring) = self.shared.ring.lock() {
            ring.extend(samples.iter().copied());
            let cap = self.shared.cap_samples;
            if ring.len() > cap {
                let drop = ring.len() - cap;
                ring.drain(..drop);
            }
        }
    }

    /// Set the playback gain (1.0 = unity, 0.0 = mute). Lock-free.
    pub fn set_volume(&self, gain: f32) {
        self.shared
            .volume_bits
            .store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Switch to a different output device, keeping the queued audio and
    /// current volume. Rebuilds the underlying stream — must be called
    /// on the owning thread.
    pub fn set_device(&mut self, device_name: &str) -> Result<(), IoError> {
        let stream = build_stream(device_name, self.sample_rate, &self.shared)?;
        stream
            .play()
            .map_err(|e| IoError::Backend(format!("monitor stream.play: {e}")))?;
        // Replacing the field drops the old stream (stops the old
        // device) only after the new one is live.
        self.stream = stream;
        self.device_name = device_name.to_string();
        Ok(())
    }

    /// Name of the device currently playing.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}

/// Build (but don't `play`) an output stream for `device_name` driven by
/// `shared`. Mirrors [`crate::cpal_sink`]'s format selection (F32 → I16
/// → U16) and mono → N-channel duplication.
fn build_stream(
    device_name: &str,
    sample_rate: u32,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, IoError> {
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
    if !matches!(
        format,
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
    ) {
        return Err(IoError::UnsupportedFormat(format!("{format:?}")));
    }
    let cfg = range.with_sample_rate(SampleRate(sample_rate));
    let channels = cfg.channels() as usize;
    let mut stream_cfg: cpal::StreamConfig = cfg.into();

    let target_frames = sample_rate * MONITOR_BUFFER_MS / 1000;
    stream_cfg.buffer_size = match supported_buf {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(target_frames.clamp(min, max)),
        SupportedBufferSize::Unknown => BufferSize::Default,
    };

    let err_cb = |e| eprintln!("[monitor] stream err: {e}");

    let build = |buf: BufferSize| {
        let mut local = stream_cfg.clone();
        local.buffer_size = buf;
        match format {
            SampleFormat::F32 => {
                let sh = shared.clone();
                device.build_output_stream::<f32, _, _>(
                    &local,
                    move |data, _| write_f32(data, channels, &sh),
                    err_cb,
                    None,
                )
            }
            SampleFormat::I16 => {
                let sh = shared.clone();
                device.build_output_stream::<i16, _, _>(
                    &local,
                    move |data, _| write_i16(data, channels, &sh),
                    err_cb,
                    None,
                )
            }
            SampleFormat::U16 => {
                let sh = shared.clone();
                device.build_output_stream::<u16, _, _>(
                    &local,
                    move |data, _| write_u16(data, channels, &sh),
                    err_cb,
                    None,
                )
            }
            // Other formats were rejected above before this closure.
            _ => unreachable!("unsupported format filtered before build_stream closure"),
        }
        .map_err(|e| IoError::Backend(format!("build_output_stream: {e}")))
    };

    match build(stream_cfg.buffer_size) {
        Ok(s) => Ok(s),
        Err(e) if matches!(stream_cfg.buffer_size, BufferSize::Fixed(_)) => {
            eprintln!("[monitor] fixed buffer refused ({e}), falling back to Default");
            build(BufferSize::Default)
        }
        Err(e) => Err(e),
    }
}

/// Pull one mono sample per frame from the ring, scale by volume, and
/// duplicate across channels. Underrun → silence.
fn drain_mono(channels: usize, frames: usize, shared: &Arc<Shared>, mut emit: impl FnMut(usize, f32)) {
    let vol = shared.volume();
    let mut ring = match shared.ring.lock() {
        Ok(r) => r,
        Err(_) => {
            for i in 0..frames * channels {
                emit(i, 0.0);
            }
            return;
        }
    };
    for f in 0..frames {
        let v = ring.pop_front().unwrap_or(0.0) * vol;
        for c in 0..channels {
            emit(f * channels + c, v);
        }
    }
}

fn write_f32(out: &mut [f32], channels: usize, shared: &Arc<Shared>) {
    let frames = out.len() / channels;
    drain_mono(channels, frames, shared, |i, v| out[i] = v);
}

fn write_i16(out: &mut [i16], channels: usize, shared: &Arc<Shared>) {
    let frames = out.len() / channels;
    drain_mono(channels, frames, shared, |i, v| {
        out[i] = (v * 32767.0).clamp(-32768.0, 32767.0) as i16;
    });
}

fn write_u16(out: &mut [u16], channels: usize, shared: &Arc<Shared>) {
    let frames = out.len() / channels;
    drain_mono(channels, frames, shared, |i, v| {
        out[i] = ((v * 32767.0).clamp(-32768.0, 32767.0) as i32 + 32768) as u16;
    });
}
