//! cpal capture thread → 48 kHz mono f32 samples channel.
//!
//! The cpal `Stream` is not `Send` on Windows (WASAPI/COM is thread-bound),
//! so we own it inside a dedicated capture thread that just parks on a stop
//! flag. Samples are forwarded to a `mpsc` channel consumed by the rx worker.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const TARGET_RATE: u32 = 48_000;

pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

pub fn start(device_name: &str) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
    let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u16), String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let device_name = device_name.to_string();

    let thread = thread::spawn(move || {
        run_capture(&device_name, sample_tx, ready_tx, stop_thread);
    });

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok((sample_rate, channels))) => Ok((
            CaptureHandle {
                stop,
                thread: Some(thread),
                sample_rate,
                channels,
            },
            sample_rx,
        )),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("timeout waiting for capture thread to start".into()),
    }
}

fn run_capture(
    device_name: &str,
    sample_tx: Sender<Vec<f32>>,
    ready_tx: Sender<Result<(u32, u16), String>>,
    stop: Arc<AtomicBool>,
) {
    let host = cpal::default_host();
    let device = match host.input_devices() {
        Ok(mut iter) => iter.find(|d| d.name().map(|n| n == device_name).unwrap_or(false)),
        Err(e) => {
            let _ = ready_tx.send(Err(format!("input_devices: {e}")));
            return;
        }
    };
    let Some(device) = device else {
        let _ = ready_tx.send(Err(format!("device '{device_name}' not found")));
        return;
    };

    let configs = match device.supported_input_configs() {
        Ok(c) => c.collect::<Vec<_>>(),
        Err(e) => {
            let _ = ready_tx.send(Err(format!("supported_input_configs: {e}")));
            return;
        }
    };
    let Some(range) = configs.into_iter().find(|c| {
        c.min_sample_rate().0 <= TARGET_RATE && TARGET_RATE <= c.max_sample_rate().0
    }) else {
        let _ = ready_tx.send(Err(format!(
            "device '{device_name}' does not support {TARGET_RATE} Hz"
        )));
        return;
    };
    let format = range.sample_format();
    let cfg = range.with_sample_rate(SampleRate(TARGET_RATE));
    let channels = cfg.channels();
    let stream_cfg: cpal::StreamConfig = cfg.into();

    let tx_cb = sample_tx.clone();
    let err_cb = |e| eprintln!("[audio_capture] stream error: {e}");

    let build_res = match format {
        SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            &stream_cfg,
            move |data, _| push_mono_f32(data, channels, &tx_cb),
            err_cb,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            &stream_cfg,
            move |data, _| push_mono_i16(data, channels, &tx_cb),
            err_cb,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            &stream_cfg,
            move |data, _| push_mono_u16(data, channels, &tx_cb),
            err_cb,
            None,
        ),
        other => {
            let _ = ready_tx.send(Err(format!("unsupported sample format: {other:?}")));
            return;
        }
    };

    let stream = match build_res {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("build_input_stream: {e}")));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(format!("stream.play: {e}")));
        return;
    }

    let _ = ready_tx.send(Ok((TARGET_RATE, channels)));

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
    // Stream dropped here → WASAPI/ALSA releases the device.
    drop(stream);
}

fn push_mono_f32(data: &[f32], channels: u16, tx: &Sender<Vec<f32>>) {
    let ch = channels as usize;
    if ch <= 1 {
        let _ = tx.send(data.to_vec());
    } else {
        let mono: Vec<f32> = data.chunks_exact(ch).map(|c| c[0]).collect();
        let _ = tx.send(mono);
    }
}

fn push_mono_i16(data: &[i16], channels: u16, tx: &Sender<Vec<f32>>) {
    let ch = channels as usize;
    const SCALE: f32 = 1.0 / 32768.0;
    let out: Vec<f32> = if ch <= 1 {
        data.iter().map(|&s| s as f32 * SCALE).collect()
    } else {
        data.chunks_exact(ch).map(|c| c[0] as f32 * SCALE).collect()
    };
    let _ = tx.send(out);
}

fn push_mono_u16(data: &[u16], channels: u16, tx: &Sender<Vec<f32>>) {
    let ch = channels as usize;
    const OFFSET: f32 = 32768.0;
    const SCALE: f32 = 1.0 / 32768.0;
    let out: Vec<f32> = if ch <= 1 {
        data.iter().map(|&s| (s as f32 - OFFSET) * SCALE).collect()
    } else {
        data.chunks_exact(ch)
            .map(|c| (c[0] as f32 - OFFSET) * SCALE)
            .collect()
    };
    let _ = tx.send(out);
}
