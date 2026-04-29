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
    raptorq_codec::k_from_payload,
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

use crate::ptt::{SharedPtt, PTT_GUARD_MS};

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
        // HIGH+ promu standard depuis 2026-04-28 (validé OTA HB9MM).
        "HIGH+" | "HIGHPLUS" => Ok(profile::profile_high_plus()),
        // EXPERIMENTAL — décodable seulement par un pair en mode forcé sur
        // le même profil.
        "FAST" => Ok(profile::profile_fast()),
        "HIGH++" | "HIGHPLUSPLUS" => Ok(profile::profile_high_plus_plus()),
        "HIGH56" | "HIGH-56" => Ok(profile::profile_high_5_6()),
        "HIGH+56" | "HIGHPLUS56" => Ok(profile::profile_high_plus_5_6()),
        _ => Err(format!("unknown profile '{name}'")),
    }
}

/// Per-profile transmission plan, derived purely from arithmetic (no symbol
/// synthesis). Computed before any TX to pre-populate the UI buttons and
/// progress bars, and to surface the RaptorQ fountain math to the user.
pub struct TxPlan {
    /// Number of LDPC codewords required at RX to reconstruct the payload
    /// (the RaptorQ "K" source-symbol count). The RX must accumulate at
    /// least this many unique ESIs before `try_decode` succeeds.
    pub k_source: u32,
    /// Number of codewords the initial TX burst actually emits (K + default
    /// repair ≈ 30 %). The progress bar counts up to here.
    pub n_initial: u32,
    /// Seconds of audio needed to transmit `n_initial` codewords at this
    /// profile's net bitrate. Includes pilot + LDPC overhead.
    pub duration_s_initial: f64,
    /// Seconds of audio needed just for `k_source` codewords — the minimum
    /// theoretical time before RX could decode if no packet was ever lost.
    pub duration_s_k: f64,
    /// Seconds per one additional codeword at this profile — used by the UI
    /// to convert "+N% More" to a duration estimate.
    pub seconds_per_cw: f64,
}

/// Compute the transmission plan for a given payload + profile + chosen
/// RaptorQ repair percentage. Inclut tous les overheads de trame (préambule,
/// header, meta, markers, pilotes, runout, EOT, plus les réinsertions
/// périodiques toutes les `V3_PREAMBLE_PERIOD_S`) pour que la durée affichée
/// dans l'UI matche ce qui passe réellement à l'antenne. Les valeurs précises
/// sortent de `frame::superframe_total_symbols` + `frame::eot_frame_symbols`,
/// qui miroitent à l'identique le builder de trame.
pub fn tx_plan(
    payload_bytes: usize,
    mode_name: &str,
    callsign_len: usize,
    filename_len: usize,
    repair_pct: u32,
) -> Result<TxPlan, String> {
    let config = parse_profile(mode_name)?;
    let envelope_overhead = 2 + filename_len + 1 + callsign_len + 1;
    let wire = payload_bytes + envelope_overhead;
    let k_bytes = config.ldpc_rate.k() / 8;
    let k_source = k_from_payload(wire, k_bytes) as u32;
    // Aligner sur ce que build_superframe_v3_range va réellement émettre :
    // si n_initial brut est impair, le builder l'arrondit pour que le
    // dernier data segment soit complet (sinon le RX perd ce CW final).
    let n_initial =
        modem_core::frame::effective_packet_count(k_source + (k_source * repair_pct) / 100);
    // Asymptote payload-only : utile pour l'estimation marginale "+N blocs".
    let bits_per_cw = (k_bytes as f64) * 8.0;
    let seconds_per_cw = bits_per_cw / config.net_bitrate();

    // Durée réelle = symboles totaux du superframe + symboles EOT, divisés par
    // le débit symbole. Capture les réinsertions périodiques (très lourdes en
    // ULTRA où une réinsertion par segment) et tous les overheads structurels.
    let total_syms_initial =
        modem_core::frame::superframe_total_symbols(&config, n_initial)
            + modem_core::frame::eot_frame_symbols(&config);
    let duration_s_initial = total_syms_initial as f64 / config.symbol_rate;
    // Pour duration_s_k : durée jusqu'au K-ième codeword, sans EOT (le RX
    // peut décoder dès K reçus, avant l'EOT).
    let duration_s_k =
        modem_core::frame::superframe_total_symbols(&config, k_source) as f64
            / config.symbol_rate;
    Ok(TxPlan {
        k_source,
        n_initial,
        duration_s_initial,
        duration_s_k,
        seconds_per_cw,
    })
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
    repair_pct: u32,
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
        .arg("--repair-pct")
        .arg(repair_pct.to_string())
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
    count: u32,
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
        .arg("--count")
        .arg(count.to_string())
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
    count: u32,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    ptt: SharedPtt,
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
            &cli, &avif_path, &wav_path, &mode, &callsign, &filename, esi_start, count,
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
        let total_blocks = count;
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
            attenuation_db,
            preemphasis_enabled,
            stop_thread,
            ptt,
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
    repair_pct: u32,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    ptt: SharedPtt,
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
        let total_blocks = match tx_plan(
            payload_bytes,
            &mode,
            callsign.len(),
            filename.len(),
            repair_pct,
        ) {
            Ok(p) => p.n_initial,
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

        if let Err(e) = generate_wav_via_cli(
            &cli, &avif_path, &wav_path, &mode, &callsign, &filename, repair_pct,
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
            attenuation_db,
            preemphasis_enabled,
            stop_thread,
            ptt,
            app,
        );
    });
    TxHandle {
        stop,
        thread: Some(thread),
    }
}

/// Pré-emphase numérique +6 dB/octave pour NBFM : shelf 1ᵉʳ ordre
/// H(s) = (1+sτ₁)/(1+sτ₂), τ₁ = 750 µs (zéro de boost à f₁ ≈ 212 Hz),
/// τ₂ = 75 µs (pôle à f₂ ≈ 2.12 kHz qui plafonne le gain HF à
/// τ₁/τ₂ = 20 dB). Bilinear transform à 48 kHz, coefficients normalisés
/// pour DC = 1.
///
/// Réponse digitale :
///   - DC : 0 dB
///   - 1 kHz : ~+13 dB
///   - 2.7 kHz : ~+18 dB
///   - Nyquist : +20 dB (plateau du shelf)
///
/// Boost important sur tout l'audio utile (NBFM commence à pré-accentuer
/// dès 200 Hz, pas 2 kHz comme la FM broadcast). L'appelant DOIT
/// peak-normaliser le signal après filtrage : sinon la carte son écrête.
fn preemphasis_nbfm_48k(samples: &mut [f32]) {
    // Bilinear sans prewarp : 2τ₁/T = 72.0, 2τ₂/T = 7.2.
    // Num brut : 73 - 71 z⁻¹  ;  Den brut : 8.2 - 6.2 z⁻¹.
    // Normalisation par a0 = 8.2 → b0 = 8.9024, b1 = -8.6585, a1 = -0.7561.
    // Pôle à z = 0.7561, stable.
    const B0: f32 = 8.9024;
    const B1: f32 = -8.6585;
    const A1: f32 = -0.7561;
    let mut x_prev = 0.0f32;
    let mut y_prev = 0.0f32;
    for s in samples.iter_mut() {
        let x = *s;
        let y = B0 * x + B1 * x_prev - A1 * y_prev;
        x_prev = x;
        y_prev = y;
        *s = y;
    }
}

/// Bascule la PTT sur la polarité TX. Best-effort : si l'écriture sur le
/// port échoue (câble débranché en cours de session…) on log et on continue,
/// le worker n'a pas vocation à interrompre la transmission pour ça.
fn ptt_engage(ptt: &SharedPtt) -> bool {
    let mut g = match ptt.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let Some(ctrl) = g.as_mut() else { return false };
    match ctrl.set_tx() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[ptt] set_tx: {e}");
            false
        }
    }
}

fn ptt_release(ptt: &SharedPtt) {
    let mut g = match ptt.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(ctrl) = g.as_mut() {
        if let Err(e) = ctrl.set_rx() {
            eprintln!("[ptt] set_rx: {e}");
        }
    }
}

fn run_playback(
    device_name: &str,
    mut samples: Vec<f32>,
    total_blocks: u32,
    duration_s: f64,
    wav_path: String,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    stop: Arc<AtomicBool>,
    ptt: SharedPtt,
    app: AppHandle,
) {
    // Pré-emphase NBFM optionnelle (+6 dB/oct, τ = 750 µs). Appliquée AVANT
    // l'atténuation pour que le pic re-normalisé respecte ensuite la consigne
    // ATT. Le shelf élève fortement les hautes fréquences audio (+13 dB à
    // 1 kHz, +18 dB à 2.7 kHz, plafond +20 dB) : sans re-normalisation le
    // signal écrêterait la carte son.
    if preemphasis_enabled {
        preemphasis_nbfm_48k(&mut samples);
        // Re-peak-normalize au même niveau que la sortie modem-cli (PEAK_NORMALIZE
        // = 0.9 dans modem-core::types). Garde un headroom de ~0.9 dB avant
        // saturation int16/F32 quelle que soit l'élévation HF du shelf.
        let peak = samples.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        if peak > 0.9 {
            let scale = 0.9 / peak;
            for s in samples.iter_mut() {
                *s *= scale;
            }
        }
    }
    // Applique l'atténuation de la cascade ATT (onglet Canal). Clamp à
    // [-60, 0] dB par sécurité — au-delà ça ne sert à rien et un signe
    // positif inattendu saturerait la carte son.
    let att_db = attenuation_db.clamp(-60.0, 0.0);
    if att_db < 0.0 {
        let gain = 10f32.powf(att_db / 20.0);
        for s in samples.iter_mut() {
            *s *= gain;
        }
    }
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
    // PTT : on bascule en émission AVANT d'ouvrir le flux audio, et on
    // attend 200 ms pour laisser le temps au transceiver de commuter.
    let ptt_engaged = ptt_engage(&ptt);
    if ptt_engaged {
        thread::sleep(Duration::from_millis(PTT_GUARD_MS));
    }

    if let Err(e) = stream.play() {
        if ptt_engaged {
            ptt_release(&ptt);
        }
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
    // 200 ms de silence avant de relâcher la PTT, puis bascule RX.
    if ptt_engaged {
        thread::sleep(Duration::from_millis(PTT_GUARD_MS));
        ptt_release(&ptt);
    }
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

/// Archive le fichier payload TX dans `<save_dir>/tx_history/` au moment où
/// l'utilisateur lance une émission. Garantit que l'historique TX trace
/// toute tentative, y compris celles coupées en cours (PTT relâché, erreur
/// audio). Émet `tx_archived` au frontend, et purge les plus anciens si on
/// dépasse `max_items`.
///
/// `payload_path` doit pointer sur un fichier existant (`tx_preview.avif` ou
/// `tx_preview.zst`). `filename` est le nom original choisi par l'utilisateur,
/// préservé tel quel dans le metadata pour l'affichage de la vignette.
pub fn archive_payload(
    save_dir: &Path,
    payload_path: &Path,
    mode: &str,
    filename: &str,
    repair_pct: u32,
    max_items: u32,
    app: &AppHandle,
) {
    let history_dir = save_dir.join("tx_history");
    if let Err(e) = std::fs::create_dir_all(&history_dir) {
        eprintln!("[tx_history] mkdir {:?}: {e}", history_dir);
        return;
    }
    let ext = payload_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("bin");
    let mime_type: u8 = match ext {
        "avif" => modem_core::app_header::mime::IMAGE_AVIF,
        "zst" => modem_core::app_header::mime::ZSTD,
        _ => modem_core::app_header::mime::BINARY,
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let safe = sanitize_filename(filename);
    let stem = format!("{ts}-{safe}");
    let archive_path = history_dir.join(format!("{stem}.{ext}"));
    let meta_path = history_dir.join(format!("{stem}.json"));

    if let Err(e) = std::fs::copy(payload_path, &archive_path) {
        eprintln!("[tx_history] copy {:?}: {e}", payload_path);
        return;
    }
    let meta = serde_json::json!({
        "timestamp": ts,
        "mode": mode,
        "mime_type": mime_type,
        "filename": filename,
        "repair_pct": repair_pct,
    });
    if let Err(e) = std::fs::write(&meta_path, meta.to_string()) {
        eprintln!("[tx_history] write meta {:?}: {e}", meta_path);
    }
    purge_history(&history_dir, max_items);
    let _ = app.emit("tx_archived", ());
}

/// Limite le dossier `tx_history/` à `max_items` triplets fichier+json. Trie
/// les `.json` par mtime descendant et supprime les plus anciens (avec leur
/// fichier source jumeau).
fn purge_history(history_dir: &Path, max_items: u32) {
    let max = max_items.max(1) as usize;
    let mut metas: Vec<(SystemTime, PathBuf)> = match std::fs::read_dir(history_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
            })
            .filter_map(|e| {
                let m = e.metadata().ok()?.modified().ok()?;
                Some((m, e.path()))
            })
            .collect(),
        Err(_) => return,
    };
    if metas.len() <= max {
        return;
    }
    metas.sort_by(|a, b| b.0.cmp(&a.0)); // plus récent d'abord
    for (_, json_path) in metas.into_iter().skip(max) {
        if let Some(stem) = json_path.file_stem().and_then(|s| s.to_str()) {
            if let Some(parent) = json_path.parent() {
                for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
                    let p = entry.path();
                    if p.file_stem().and_then(|s| s.to_str()) == Some(stem) {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }
}

/// Sanitize un nom de fichier pour le système de fichiers : remplace les
/// caractères réservés Windows/Linux par `_`, tronque à 80 caractères pour
/// laisser de la place au préfixe timestamp + extension.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        return "fichier".to_string();
    }
    if trimmed.len() > 80 {
        trimmed.chars().take(80).collect()
    } else {
        trimmed
    }
}
