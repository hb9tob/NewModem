//! cpal capture thread → 48 kHz mono f32 samples channel.
//!
//! The cpal `Stream` is not `Send` on Windows (WASAPI/COM is thread-bound),
//! so we own it inside a dedicated capture thread that just parks on a stop
//! flag. Samples are forwarded to a `mpsc` channel consumed by the rx worker.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TARGET_RATE: u32 = 48_000;

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("nbfm-capture.log")
}

fn log(msg: &str) {
    eprintln!("{msg}");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{msg}");
    }
}

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
    // Keep only configs that cover 48 kHz, then pick by format preference :
    // F32 > I16 > U16 > U8 (mirror of what we can decode). This avoids
    // picking an unsupported format first when the device also advertises
    // a usable one.
    let supports_48k: Vec<_> = configs
        .into_iter()
        .filter(|c| c.min_sample_rate().0 <= TARGET_RATE && TARGET_RATE <= c.max_sample_rate().0)
        .collect();
    if supports_48k.is_empty() {
        let _ = ready_tx.send(Err(format!(
            "device '{device_name}' does not support {TARGET_RATE} Hz"
        )));
        return;
    }
    fn rank(f: SampleFormat) -> u8 {
        match f {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            SampleFormat::U16 => 2,
            SampleFormat::U8 => 3,
            _ => 4,
        }
    }
    let range = supports_48k
        .into_iter()
        .min_by_key(|c| rank(c.sample_format()))
        .unwrap();
    let format = range.sample_format();
    let cfg = range.with_sample_rate(SampleRate(TARGET_RATE));
    let channels = cfg.channels();
    let stream_cfg: cpal::StreamConfig = cfg.into();

    let _ = std::fs::remove_file(log_path());
    log(&format!(
        "[capture] building stream : device='{device_name}' format={format:?} channels={channels} target={TARGET_RATE}Hz"
    ));

    let callback_count = Arc::new(AtomicU64::new(0));
    let callback_samples = Arc::new(AtomicU64::new(0));
    let cb_count = callback_count.clone();
    let cb_samples = callback_samples.clone();

    let tx_cb = sample_tx.clone();
    let err_cb = |e| {
        log(&format!("[capture] stream error: {e}"));
    };

    let build_res = match format {
        SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            &stream_cfg,
            move |data, _| {
                cb_count.fetch_add(1, Ordering::Relaxed);
                cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                push_mono_f32(data, channels, &tx_cb);
            },
            err_cb,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            &stream_cfg,
            move |data, _| {
                cb_count.fetch_add(1, Ordering::Relaxed);
                cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                push_mono_i16(data, channels, &tx_cb);
            },
            err_cb,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            &stream_cfg,
            move |data, _| {
                cb_count.fetch_add(1, Ordering::Relaxed);
                cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                push_mono_u16(data, channels, &tx_cb);
            },
            err_cb,
            None,
        ),
        SampleFormat::U8 => device.build_input_stream::<u8, _, _>(
            &stream_cfg,
            move |data, _| {
                cb_count.fetch_add(1, Ordering::Relaxed);
                cb_samples.fetch_add(data.len() as u64, Ordering::Relaxed);
                push_mono_u8(data, channels, &tx_cb);
            },
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

    log(&format!(
        "[capture] stream.play OK — device='{device_name}' rate={TARGET_RATE} channels={channels} format={format:?} buffer_size={:?}",
        stream_cfg.buffer_size
    ));
    let _ = ready_tx.send(Ok((TARGET_RATE, channels)));

    let mut last_tick = Instant::now();
    let mut last_samples: u64 = 0;
    let mut last_count: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        let now = Instant::now();
        if now.duration_since(last_tick) >= Duration::from_secs(2) {
            let cnt = callback_count.load(Ordering::Relaxed);
            let smp = callback_samples.load(Ordering::Relaxed);
            let dt = now.duration_since(last_tick).as_secs_f64();
            let cb_per_sec = (cnt - last_count) as f64 / dt;
            let smp_per_sec = (smp - last_samples) as f64 / dt;
            log(&format!(
                "[capture] tick: cbs={cnt} (+{} in {:.1}s, {:.1}/s), samples={smp} ({:.0} Sa/s ≈ {:.1}× target)",
                cnt - last_count,
                dt,
                cb_per_sec,
                smp_per_sec,
                smp_per_sec / (TARGET_RATE as f64 * channels as f64)
            ));
            last_tick = now;
            last_count = cnt;
            last_samples = smp;
        }
    }
    log("[capture] stop flag → dropping stream");
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

fn push_mono_u8(data: &[u8], channels: u16, tx: &Sender<Vec<f32>>) {
    let ch = channels as usize;
    const OFFSET: f32 = 128.0;
    const SCALE: f32 = 1.0 / 128.0;
    let out: Vec<f32> = if ch <= 1 {
        data.iter().map(|&s| (s as f32 - OFFSET) * SCALE).collect()
    } else {
        data.chunks_exact(ch)
            .map(|c| (c[0] as f32 - OFFSET) * SCALE)
            .collect()
    };
    let _ = tx.send(out);
}
