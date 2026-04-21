//! TX worker : génération du WAV via le binaire modem-cli, puis playback
//! sur la carte son sélectionnée.
//!
//! Pipeline (thread dédié, non bloquant pour Tauri) :
//!   1. Estime durée / nb de blocs (helper pour la validation UI côté JS).
//!   2. Spawn `nbfm-modem tx --input <avif> --output <wav> --frame-version 3
//!                           --profile <MODE> --callsign <QRZ> --filename <...>`
//!   3. Lit le WAV produit avec hound, décode en f32.
//!   4. Joue via cpal sur le device TX choisi.
//!
//! Événements (AppHandle::emit) :
//!   - tx_plan      { duration_s, total_blocks, wire_bytes, wav_path,
//!                    mode, callsign, filename }
//!   - tx_progress  { pos_samples, total_samples, elapsed_s, duration_s,
//!                    blocks_sent (interp. linéaire), total_blocks }
//!   - tx_complete  { duration_s, wav_path, stopped_early }
//!   - tx_error     { message }

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate};
use hound::{SampleFormat as WavFmt, WavReader};
use modem_core::{
    profile::{self, ModemConfig},
    types::AUDIO_RATE,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

#[derive(Serialize, Clone)]
pub struct TxPlanEvent {
    pub duration_s: f64,
    pub total_blocks: u32,
    pub wire_bytes: u32,
    pub wav_path: String,
    pub mode: String,
    pub callsign: String,
    pub filename: String,
}

#[derive(Serialize, Clone)]
pub struct TxProgressEvent {
    pub pos_samples: u64,
    pub total_samples: u64,
    pub elapsed_s: f64,
    pub duration_s: f64,
    pub blocks_sent: u32,
    pub total_blocks: u32,
}

#[derive(Serialize, Clone)]
pub struct TxCompleteEvent {
    pub duration_s: f64,
    pub wav_path: String,
    pub stopped_early: bool,
}

#[derive(Serialize, Clone)]
pub struct TxErrorEvent {
    pub message: String,
}

pub struct TxHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl TxHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

pub fn parse_profile(name: &str) -> Result<ModemConfig, String> {
    match name.to_uppercase().as_str() {
        "MEGA" => Ok(profile::profile_mega()),
        "HIGH" => Ok(profile::profile_high()),
        "NORMAL" => Ok(profile::profile_normal()),
        "ROBUST" => Ok(profile::profile_robust()),
        "ULTRA" => Ok(profile::profile_ultra()),
        _ => Err(format!("unknown profile '{name}'")),
    }
}

/// Estimation rapide (helper pour les garde-fous UI côté JS) : durée en
/// secondes à partir de la taille payload (octets AVIF) + mode + longueur
/// filename/callsign. Inclut le surcoût d'envelope (2 + filename + 1 +
/// callsign + 1) et l'arrondi à la frontière codeword LDPC. Ne construit
/// pas les symboles — purement arithmétique.
pub fn estimate_duration_s(
    payload_bytes: usize,
    mode_name: &str,
    callsign_len: usize,
    filename_len: usize,
) -> Result<f64, String> {
    let config = parse_profile(mode_name)?;
    let envelope_overhead = 2 + filename_len + 1 + callsign_len + 1;
    let wire = payload_bytes + envelope_overhead;
    let k_bytes = config.ldpc_rate.k() / 8;
    let padded = wire.div_ceil(k_bytes) * k_bytes;
    let bits = (padded as f64) * 8.0;
    Ok(bits / config.net_bitrate())
}

pub fn estimate_total_blocks(
    payload_bytes: usize,
    mode_name: &str,
    callsign_len: usize,
    filename_len: usize,
) -> Result<u32, String> {
    let config = parse_profile(mode_name)?;
    let envelope_overhead = 2 + filename_len + 1 + callsign_len + 1;
    let wire = payload_bytes + envelope_overhead;
    let k_bytes = config.ldpc_rate.k() / 8;
    Ok(wire.div_ceil(k_bytes) as u32)
}

/// Retrouve le binaire modem-cli à côté du GUI. Priorité :
///   1. `nbfm-modem-<TARGET_TRIPLE>[.exe]` — nom produit par le sidecar
///      Tauri (`externalBin`), installé dans `/usr/bin/` par le .deb.
///   2. `nbfm-modem[.exe]` — nom brut (dev workspace `target/release/`).
fn locate_cli_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let triple = env!("TARGET_TRIPLE");
    let sidecar = dir.join(format!("nbfm-modem-{triple}{ext}"));
    if sidecar.exists() {
        return Some(sidecar);
    }
    let bare = dir.join(format!("nbfm-modem{ext}"));
    if bare.exists() {
        return Some(bare);
    }
    None
}

fn generate_wav_via_cli(
    cli: &Path,
    input_avif: &Path,
    output_wav: &Path,
    mode: &str,
    callsign: &str,
    filename: &str,
) -> Result<(), String> {
    let output = Command::new(cli)
        .arg("tx")
        .arg("--input")
        .arg(input_avif)
        .arg("--output")
        .arg(output_wav)
        .arg("--profile")
        .arg(mode)
        .arg("--callsign")
        .arg(callsign)
        .arg("--filename")
        .arg(filename)
        .output()
        .map_err(|e| format!("spawn modem-cli: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "modem-cli tx exit {:?} — {}",
            output.status.code(),
            stderr.trim()
        ));
    }
    Ok(())
}

fn generate_wav_more_via_cli(
    cli: &Path,
    input_avif: &Path,
    output_wav: &Path,
    mode: &str,
    callsign: &str,
    filename: &str,
    esi_start: u32,
    pct: u32,
) -> Result<(), String> {
    let output = Command::new(cli)
        .arg("tx-more")
        .arg("--input")
        .arg(input_avif)
        .arg("--output")
        .arg(output_wav)
        .arg("--profile")
        .arg(mode)
        .arg("--callsign")
        .arg(callsign)
        .arg("--filename")
        .arg(filename)
        .arg("--esi-start")
        .arg(esi_start.to_string())
        .arg("--pct")
        .arg(pct.to_string())
        .output()
        .map_err(|e| format!("spawn modem-cli tx-more: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "modem-cli tx-more exit {:?} — {}",
            output.status.code(),
            stderr.trim()
        ));
    }
    Ok(())
}

fn read_wav_samples(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader = WavReader::open(path).map_err(|e| format!("wav open: {e}"))?;
    let spec = reader.spec();
    if spec.channels != 1 {
        return Err(format!("wav not mono (channels={})", spec.channels));
    }
    if spec.sample_rate != AUDIO_RATE {
        return Err(format!(
            "wav sample_rate {} != {}",
            spec.sample_rate, AUDIO_RATE
        ));
    }
    match spec.sample_format {
        WavFmt::Int => {
            let max = (1u32 << (spec.bits_per_sample - 1)) as f32;
            Ok(reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max).unwrap_or(0.0))
                .collect())
        }
        WavFmt::Float => Ok(reader
            .samples::<f32>()
            .map(|s| s.unwrap_or(0.0))
            .collect()),
    }
}

/// Burst variant : initial (`esi_start=None`) or "More" (`esi_start=Some,
/// pct`). Both paths share `run_playback` for the cpal stream.
pub fn spawn_more(
    avif_path: PathBuf,
    mode: String,
    callsign: String,
    filename: String,
    device_name: String,
    save_dir: PathBuf,
    esi_start: u32,
    pct: u32,
    app: AppHandle,
) -> TxHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        let Some(cli) = locate_cli_binary() else {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: "binaire nbfm-modem introuvable à côté du GUI".to_string(),
                },
            );
            return;
        };
        if !avif_path.exists() {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("AVIF absent : {}", avif_path.display()),
                },
            );
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&save_dir) {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("mkdir save_dir: {e}"),
                },
            );
            return;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let wav_path = save_dir.join(format!(
            "tx-more-{ts}-{}-esi{esi_start}.wav",
            mode.to_lowercase()
        ));
        if let Err(e) = generate_wav_more_via_cli(
            &cli, &avif_path, &wav_path, &mode, &callsign, &filename, esi_start, pct,
        ) {
            let _ = app.emit("tx_error", TxErrorEvent { message: e });
            return;
        }
        let samples = match read_wav_samples(&wav_path) {
            Ok(v) => v,
            Err(e) => {
                let _ = app.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
        let wav_str = wav_path.to_string_lossy().into_owned();
        // total_blocks for the More burst is "roughly K * pct / 100" — we
        // can't reconstruct K here without parsing the AVIF, so expose an
        // approximation that the UI can tolerate (exact value is already in
        // the CLI's stderr log for debug).
        let total_blocks = 1u32; // UI will ignore the exact count for More bursts
        let _ = app.emit(
            "tx_plan",
            TxPlanEvent {
                duration_s,
                total_blocks,
                wire_bytes: 0,
                wav_path: wav_str.clone(),
                mode: mode.clone(),
                callsign: callsign.clone(),
                filename: filename.clone(),
            },
        );
        run_playback(
            &device_name,
            samples,
            total_blocks,
            duration_s,
            wav_str,
            stop_thread,
            app,
        );
    });
    TxHandle {
        stop,
        thread: Some(thread),
    }
}

pub fn spawn(
    avif_path: PathBuf,
    mode: String,
    callsign: String,
    filename: String,
    device_name: String,
    save_dir: PathBuf,
    app: AppHandle,
) -> TxHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        let Some(cli) = locate_cli_binary() else {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: "binaire nbfm-modem introuvable à côté du GUI".to_string(),
                },
            );
            return;
        };
        if !avif_path.exists() {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("AVIF absent : {}", avif_path.display()),
                },
            );
            return;
        }
        let payload_bytes = match std::fs::metadata(&avif_path) {
            Ok(m) => m.len() as usize,
            Err(e) => {
                let _ = app.emit(
                    "tx_error",
                    TxErrorEvent {
                        message: format!("metadata avif: {e}"),
                    },
                );
                return;
            }
        };
        let total_blocks =
            match estimate_total_blocks(payload_bytes, &mode, callsign.len(), filename.len()) {
                Ok(v) => v,
                Err(e) => {
                    let _ = app.emit("tx_error", TxErrorEvent { message: e });
                    return;
                }
            };

        if let Err(e) = std::fs::create_dir_all(&save_dir) {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("mkdir save_dir: {e}"),
                },
            );
            return;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let wav_path = save_dir.join(format!("tx-{ts}-{}.wav", mode.to_lowercase()));

        if let Err(e) =
            generate_wav_via_cli(&cli, &avif_path, &wav_path, &mode, &callsign, &filename)
        {
            let _ = app.emit("tx_error", TxErrorEvent { message: e });
            return;
        }

        let samples = match read_wav_samples(&wav_path) {
            Ok(v) => v,
            Err(e) => {
                let _ = app.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
        let wire_bytes = payload_bytes as u32; // approx — CLI ne remonte pas la taille envelope
        let wav_str = wav_path.to_string_lossy().into_owned();

        let _ = app.emit(
            "tx_plan",
            TxPlanEvent {
                duration_s,
                total_blocks,
                wire_bytes,
                wav_path: wav_str.clone(),
                mode: mode.clone(),
                callsign: callsign.clone(),
                filename: filename.clone(),
            },
        );

        run_playback(
            &device_name,
            samples,
            total_blocks,
            duration_s,
            wav_str,
            stop_thread,
            app,
        );
    });
    TxHandle {
        stop,
        thread: Some(thread),
    }
}

fn run_playback(
    device_name: &str,
    samples: Vec<f32>,
    total_blocks: u32,
    duration_s: f64,
    wav_path: String,
    stop: Arc<AtomicBool>,
    app: AppHandle,
) {
    let total_samples = samples.len();

    let host = cpal::default_host();
    let device = match host.output_devices() {
        Ok(mut it) => it.find(|d| d.name().map(|n| n == device_name).unwrap_or(false)),
        Err(e) => {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("output_devices: {e}"),
                },
            );
            return;
        }
    };
    let Some(device) = device else {
        let _ = app.emit(
            "tx_error",
            TxErrorEvent {
                message: format!("TX device '{device_name}' not found"),
            },
        );
        return;
    };

    let configs = match device.supported_output_configs() {
        Ok(c) => c.collect::<Vec<_>>(),
        Err(e) => {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("supported_output_configs: {e}"),
                },
            );
            return;
        }
    };
    let supports_48k: Vec<_> = configs
        .into_iter()
        .filter(|c| c.min_sample_rate().0 <= AUDIO_RATE && AUDIO_RATE <= c.max_sample_rate().0)
        .collect();
    if supports_48k.is_empty() {
        let _ = app.emit(
            "tx_error",
            TxErrorEvent {
                message: format!("TX device '{device_name}' does not support {AUDIO_RATE} Hz"),
            },
        );
        return;
    }
    fn rank(f: SampleFormat) -> u8 {
        match f {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            SampleFormat::U16 => 2,
            _ => 4,
        }
    }
    let range = supports_48k
        .into_iter()
        .min_by_key(|c| rank(c.sample_format()))
        .unwrap();
    let format = range.sample_format();
    let cfg = range.with_sample_rate(SampleRate(AUDIO_RATE));
    let channels = cfg.channels() as usize;
    let stream_cfg: cpal::StreamConfig = cfg.into();

    let pos = Arc::new(AtomicUsize::new(0));
    let pos_cb = pos.clone();
    let err_cb = |e| eprintln!("[tx] stream err: {e}");

    let samples_arc: Arc<[f32]> = samples.into();
    let s_f32 = samples_arc.clone();
    let s_i16 = samples_arc.clone();
    let s_u16 = samples_arc.clone();

    let build = match format {
        SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            &stream_cfg,
            move |data, _| write_out_f32(data, channels, &s_f32, &pos_cb),
            err_cb,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            &stream_cfg,
            move |data, _| write_out_i16(data, channels, &s_i16, &pos_cb),
            err_cb,
            None,
        ),
        SampleFormat::U16 => device.build_output_stream::<u16, _, _>(
            &stream_cfg,
            move |data, _| write_out_u16(data, channels, &s_u16, &pos_cb),
            err_cb,
            None,
        ),
        other => {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("unsupported output format: {other:?}"),
                },
            );
            return;
        }
    };
    let stream = match build {
        Ok(s) => s,
        Err(e) => {
            let _ = app.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("build_output_stream: {e}"),
                },
            );
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = app.emit(
            "tx_error",
            TxErrorEvent {
                message: format!("stream.play: {e}"),
            },
        );
        return;
    }

    let start = Instant::now();
    let mut last_tick = Instant::now() - Duration::from_millis(300);
    let mut stopped_early = false;
    loop {
        thread::sleep(Duration::from_millis(100));
        let p = pos.load(Ordering::Relaxed);
        let done = p >= total_samples;
        if stop.load(Ordering::Relaxed) {
            stopped_early = true;
        }
        let now = Instant::now();
        let should_emit =
            now.duration_since(last_tick) >= Duration::from_millis(200) || done || stopped_early;
        if should_emit {
            let elapsed_s = start.elapsed().as_secs_f64();
            let capped = p.min(total_samples);
            let frac = if total_samples > 0 {
                capped as f64 / total_samples as f64
            } else {
                1.0
            };
            let blocks_sent = ((frac * total_blocks as f64).round() as u32).min(total_blocks);
            let _ = app.emit(
                "tx_progress",
                TxProgressEvent {
                    pos_samples: capped as u64,
                    total_samples: total_samples as u64,
                    elapsed_s,
                    duration_s,
                    blocks_sent,
                    total_blocks,
                },
            );
            last_tick = now;
        }
        if done || stopped_early {
            break;
        }
    }

    drop(stream);
    let _ = app.emit(
        "tx_complete",
        TxCompleteEvent {
            duration_s: start.elapsed().as_secs_f64(),
            wav_path,
            stopped_early,
        },
    );
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
