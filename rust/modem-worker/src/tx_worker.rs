//! TX worker: synthesises the audio in-process via `V3Modem.encode_to_samples`
//! and plays it back on the selected sound card. Phase 2D-ii dropped the
//! legacy subprocess CLI pipeline (was: `nbfm-modem tx --input <avif>
//! --output <wav>`, then read back the WAV).
//!
//! Pipeline (dedicated thread, non-blocking for Tauri):
//!   1. Read the source bytes (AVIF or zstd, produced by tx_encode).
//!   2. Wrap in PayloadEnvelope (filename + callsign + content).
//!   3. Plan RaptorQ: K + repair → n_total packets.
//!   4. Build EncodeRequest, call V3Modem.encode_to_samples(req).
//!      VOX preamble stays at 0.5 s (radios without PTT need it —
//!      memory feedback_keep_vox).
//!   5. Play the resulting f32 samples via cpal on the chosen TX device.
//!
//! Events (sink.emit):
//!   - tx_plan      { duration_s, total_blocks, wire_bytes, wav_path,
//!                    mode, callsign, filename }
//!     `wav_path` is now empty (no intermediate file).
//!   - tx_progress  { pos_samples, total_samples, elapsed_s, duration_s,
//!                    blocks_sent (linearly interpolated), total_blocks }
//!   - tx_complete  { duration_s, wav_path, stopped_early }
//!   - tx_error     { message }

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate};
use modem_core::{
    app_header::{self, mime},
    payload_envelope::PayloadEnvelope,
    profile::{self, ModemConfig, ProfileIndex},
    raptorq_codec::k_from_payload,
    traits::{EncodeRequest, Modem},
    types::AUDIO_RATE,
    v3_modem::V3Modem,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::event_sink::{EventSink, EventSinkExt};
use crate::ptt::{SharedPtt, PTT_GUARD_MS};

/// VOX preamble duration passed to `V3Modem.encode_to_samples`. 0.5 s
/// matches what `nbfm-modem tx` writes by default (`--vox 0.5`) and what
/// the GUI played on air before the in-process migration. Required for
/// stations whose transceiver triggers TX via VOX rather than a wired PTT.
const TX_VOX_SECONDS: f64 = 0.5;

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
    profile::config_by_name(name).ok_or_else(|| format!("unknown profile '{name}'"))
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
/// RaptorQ repair percentage. Includes every frame overhead (preamble,
/// header, meta, markers, pilots, runout, EOT, plus periodic re-insertions
/// every `V3_PREAMBLE_PERIOD_S`) so the duration shown in the UI matches
/// what actually goes on air. Exact values come from
/// `frame::superframe_total_symbols` + `frame::eot_frame_symbols`, which
/// mirror the frame builder one-to-one.
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
    // Align on what build_superframe_v3_range will actually emit: if the
    // raw n_initial is odd, the builder rounds it so the last data
    // segment is complete (otherwise RX loses that final CW).
    let n_initial =
        modem_core::frame::effective_packet_count(k_source + (k_source * repair_pct) / 100);
    // Asymptote payload-only : utile pour l'estimation marginale "+N blocs".
    let bits_per_cw = (k_bytes as f64) * 8.0;
    let seconds_per_cw = bits_per_cw / config.net_bitrate();

    // Actual duration = total superframe symbols + EOT symbols, divided
    // by the symbol rate. Captures periodic re-insertions (very costly on
    // ULTRA where there is one per segment) and every structural overhead.
    let total_syms_initial =
        modem_core::frame::superframe_total_symbols(&config, n_initial)
            + modem_core::frame::eot_frame_symbols(&config);
    let duration_s_initial = total_syms_initial as f64 / config.symbol_rate;
    // For duration_s_k: duration up to the K-th codeword, without EOT
    // (the RX can decode as soon as K are received, before the EOT).
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


/// FNV-1a 32-bit + xor-fold to 16 bits — same routine the legacy CLI
/// uses to derive the app-header `hash_short` field. Bit-for-bit
/// equivalent to `content_hash_short` in modem-cli/src/main.rs.
fn fnv_short(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

/// Map a source-file extension to the protocol-level mime byte. Mirrors
/// `infer_mime` in modem-cli/src/main.rs — the GUI sends either
/// `tx-preview.avif` (image) or `tx-preview.zst` (file mode).
fn infer_mime(path: &Path) -> u8 {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("avif") => mime::IMAGE_AVIF,
        Some("jpg") | Some("jpeg") => mime::IMAGE_JPEG,
        Some("png") => mime::IMAGE_PNG,
        Some("txt") | Some("md") => mime::TEXT,
        Some("zst") => mime::ZSTD,
        _ => mime::BINARY,
    }
}

/// Resolve a profile name to its protocol-level `ProfileIndex` byte
/// (used to seed `compute_session_id`). Returns `None` for an unknown
/// name; callers report it as a tx_error.
fn profile_index_for(name: &str) -> Option<u8> {
    ProfileIndex::from_name(name).map(ProfileIndex::as_u8)
}

/// Burst variant : initial (`esi_start=None`) or "More" (`esi_start=Some,
/// pct`). Both paths share `run_playback` for the cpal stream.
/// Continuation burst (RaptorQ "More"): encodes `count` packets starting
/// at `esi_start`, reusing the same envelope/session/mime as the initial
/// burst. The RX recognises the session by `session_id` (deterministic
/// from envelope + profile), so as long as the source file + profile +
/// callsign + filename match the initial TX, the new ESIs land in the
/// same disk session.
pub fn spawn_more(
    avif_path: PathBuf,
    mode: String,
    callsign: String,
    filename: String,
    device_name: String,
    _save_dir: PathBuf,
    esi_start: u32,
    count: u32,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    ptt: SharedPtt,
    sink: Arc<dyn EventSink>,
) -> TxHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        let samples = match encode_in_process(
            &avif_path,
            &mode,
            &callsign,
            &filename,
            esi_start,
            count,
        ) {
            Ok(s) => s,
            Err(e) => {
                sink.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
        let total_blocks = count;
        sink.emit(
            "tx_plan",
            TxPlanEvent {
                duration_s,
                total_blocks,
                wire_bytes: 0,
                wav_path: String::new(),
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
            String::new(),
            attenuation_db,
            preemphasis_enabled,
            stop_thread,
            ptt,
            sink,
        );
    });
    TxHandle {
        stop,
        thread: Some(thread),
    }
}

/// Initial TX burst: encodes K source + repair packets so the RX has
/// enough redundancy to fountain-decode without retransmission, on a
/// loss-free channel.
pub fn spawn(
    avif_path: PathBuf,
    mode: String,
    callsign: String,
    filename: String,
    device_name: String,
    _save_dir: PathBuf,
    repair_pct: u32,
    attenuation_db: f32,
    preemphasis_enabled: bool,
    ptt: SharedPtt,
    sink: Arc<dyn EventSink>,
) -> TxHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        let payload_bytes = match std::fs::metadata(&avif_path).map(|m| m.len() as usize) {
            Ok(n) => n,
            Err(e) => {
                sink.emit(
                    "tx_error",
                    TxErrorEvent {
                        message: format!("source absent ou inaccessible: {e}"),
                    },
                );
                return;
            }
        };
        let plan = match tx_plan(
            payload_bytes,
            &mode,
            callsign.len(),
            filename.len(),
            repair_pct,
        ) {
            Ok(p) => p,
            Err(e) => {
                sink.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let total_blocks = plan.n_initial;

        let samples = match encode_in_process(
            &avif_path,
            &mode,
            &callsign,
            &filename,
            0,
            total_blocks,
        ) {
            Ok(s) => s,
            Err(e) => {
                sink.emit("tx_error", TxErrorEvent { message: e });
                return;
            }
        };
        let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
        let envelope_overhead = 2 + filename.len() + 1 + callsign.len() + 1;
        let wire_bytes = (payload_bytes + envelope_overhead) as u32;

        sink.emit(
            "tx_plan",
            TxPlanEvent {
                duration_s,
                total_blocks,
                wire_bytes,
                wav_path: String::new(),
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
            String::new(),
            attenuation_db,
            preemphasis_enabled,
            stop_thread,
            ptt,
            sink,
        );
    });
    TxHandle {
        stop,
        thread: Some(thread),
    }
}

/// Read the source file, build the wire payload (envelope-wrapped),
/// derive session_id + hash + mime, and call `V3Modem.encode_to_samples`
/// to produce the audio. Used for both the initial burst (esi_start=0)
/// and the "More" continuation burst (esi_start>0).
fn encode_in_process(
    avif_path: &Path,
    mode: &str,
    callsign: &str,
    filename: &str,
    esi_start: u32,
    n_packets: u32,
) -> Result<Vec<f32>, String> {
    let payload = std::fs::read(avif_path)
        .map_err(|e| format!("read {}: {e}", avif_path.display()))?;
    let mime_type = infer_mime(avif_path);
    let envelope = PayloadEnvelope::new(filename, callsign, payload).ok_or_else(|| {
        format!(
            "envelope too large (filename={}, callsign={})",
            filename.len(),
            callsign.len()
        )
    })?;
    let wire_payload = envelope.encode();

    let cfg = parse_profile(mode)?;
    let profile_index = profile_index_for(mode)
        .ok_or_else(|| format!("unknown profile '{mode}'"))?;
    let session_id =
        app_header::compute_session_id(&wire_payload, cfg.mode_code(), profile_index);
    let hash_short = fnv_short(&wire_payload);

    let req = EncodeRequest {
        profile: mode,
        wire_payload: &wire_payload,
        session_id,
        mime_type,
        hash_short,
        esi_start,
        n_packets,
        vox_seconds: TX_VOX_SECONDS,
    };
    V3Modem
        .encode_to_samples(&req)
        .map_err(|e| format!("encode: {e}"))
}

/// Digital +6 dB/octave NBFM pre-emphasis: first-order shelf
/// H(s) = (1+s*tau1)/(1+s*tau2), tau1 = 750 us (zero at f1 ~= 212 Hz),
/// tau2 = 75 us (pole at f2 ~= 2.12 kHz, capping the HF gain at
/// tau1/tau2 = 20 dB). Bilinear transform at 48 kHz, coefficients
/// normalized for DC = 1.
///
/// Digital response:
///   - DC      : 0 dB
///   - 1 kHz   : ~+13 dB
///   - 2.7 kHz : ~+18 dB
///   - Nyquist : +20 dB (shelf plateau)
///
/// Heavy boost across the full useful audio band (NBFM starts pre-
/// emphasizing as low as 200 Hz, not 2 kHz like broadcast FM). The
/// caller MUST peak-normalize the signal after filtering, otherwise
/// the sound card clips.
fn preemphasis_nbfm_48k(samples: &mut [f32]) {
    // Bilinear sans prewarp : 2τ₁/T = 72.0, 2τ₂/T = 7.2.
    // Num brut : 73 - 71 z⁻¹  ;  Den brut : 8.2 - 6.2 z⁻¹.
    // Normalisation par a0 = 8.2 → b0 = 8.9024, b1 = -8.6585, a1 = -0.7561.
    // Pole at z = 0.7561, stable.
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

/// Switch the PTT to TX polarity. Best-effort: if writing to the port
/// fails (cable unplugged mid-session, ...) we log and keep going - the
/// worker is not in the business of aborting a transmission for that.
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
    sink: Arc<dyn EventSink>,
) {
    // Optional NBFM pre-emphasis (+6 dB/oct, tau = 750 us). Applied
    // BEFORE the attenuation so that the re-normalized peak still
    // respects the ATT setpoint. The shelf strongly lifts the high audio
    // frequencies (+13 dB at 1 kHz, +18 dB at 2.7 kHz, plateau +20 dB):
    // without re-normalization the signal would clip the sound card.
    if preemphasis_enabled {
        preemphasis_nbfm_48k(&mut samples);
        // Re-peak-normalize back to the modem-cli output level
        // (PEAK_NORMALIZE = 0.9 in modem-core::types). Keeps ~0.9 dB of
        // headroom before int16/F32 saturation regardless of the shelf
        // HF lift.
        let peak = samples.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        if peak > 0.9 {
            let scale = 0.9 / peak;
            for s in samples.iter_mut() {
                *s *= scale;
            }
        }
    }
    // Apply the ATT cascade attenuation (Channel tab). Clamp to
    // [-60, 0] dB defensively - beyond that range it serves no purpose
    // and an unexpected positive sign would saturate the sound card.
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
            sink.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("output_devices: {e}"),
                },
            );
            return;
        }
    };
    let Some(device) = device else {
        sink.emit(
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
            sink.emit(
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
        sink.emit(
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
            sink.emit(
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
            sink.emit(
                "tx_error",
                TxErrorEvent {
                    message: format!("build_output_stream: {e}"),
                },
            );
            return;
        }
    };
    // PTT: switch to TX BEFORE opening the audio stream, then wait
    // 200 ms to give the transceiver time to commute.
    let ptt_engaged = ptt_engage(&ptt);
    if ptt_engaged {
        thread::sleep(Duration::from_millis(PTT_GUARD_MS));
    }

    if let Err(e) = stream.play() {
        if ptt_engaged {
            ptt_release(&ptt);
        }
        sink.emit(
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
            sink.emit(
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
    // 200 ms of silence before releasing the PTT, then switch back to RX.
    if ptt_engaged {
        thread::sleep(Duration::from_millis(PTT_GUARD_MS));
        ptt_release(&ptt);
    }
    sink.emit(
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

/// Archive the TX payload file under `<save_dir>/tx_history/` at the
/// moment the user starts a transmission. Guarantees that the TX
/// history traces every attempt, including the ones aborted mid-burst
/// (PTT released, audio error). Emits `tx_archived` on the frontend
/// and purges the oldest entries if `max_items` is exceeded.
///
/// `payload_path` must point at an existing file (`tx_preview.avif` or
/// `tx_preview.zst`). `filename` is the original name chosen by the
/// user, preserved as-is in the metadata for thumbnail display.
pub fn archive_payload(
    save_dir: &Path,
    payload_path: &Path,
    mode: &str,
    filename: &str,
    repair_pct: u32,
    max_items: u32,
    sink: &dyn EventSink,
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
    sink.emit("tx_archived", ());
}

/// Cap the `tx_history/` folder to `max_items` file+json pairs. Sort
/// the `.json` files by descending mtime and remove the oldest ones
/// (along with their twin source file).
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
    metas.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
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

/// Sanitize a filename for the filesystem: replace Windows/Linux
/// reserved characters with `_`, and truncate to 80 characters to
/// leave room for the timestamp prefix + extension.
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
