//! In-process TX worker for the 2x family.
//!
//! Wraps [`V4Modem::encode_to_samples`] with a few orchestration helpers
//! the GUI / CLI need: write the audio to a WAV file, push it to a
//! `SampleSink` for live playback, or hand it back as a buffer for
//! further processing.
//!
//! Two layers, both family-specific:
//!
//! - [`encode_to_audio`] / [`encode_to_wav`] — synchronous wrappers
//!   around `V4Modem::encode_to_samples`. The CLI calls them inline.
//! - [`tx_plan`] / [`spawn`] / [`spawn_more`] — GUI orchestration:
//!   compute the plan, spin up a background thread, hand the rendered
//!   audio to the shared
//!   [`modem_worker_base::tx_runtime::run_playback`]. Identical event
//!   contract to the V3 sibling (`modem-worker::tx_worker::spawn`),
//!   so the Tauri layer dispatches on `mode` and the JS listeners stay
//!   the same.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;

use hound::{SampleFormat, WavSpec, WavWriter};
use modem_core_base::traits::{EncodeRequest, Modem, ModemError};
use modem_core_base::types::AUDIO_RATE;
use modem_core2x::frame2x::{eot_frame_symbols_v4, superframe_total_symbols_v4};
use modem_core2x::modem2x::V4Modem;
use modem_core2x::profile2x::config_by_name_2x;
use modem_framing::{
    app_header::{self, mime},
    payload_envelope::PayloadEnvelope,
    raptorq_codec::k_from_payload,
};
use modem_io::SampleSink;
use modem_worker_base::tx_runtime::{
    build_tx_wav_path, run_playback, TxErrorEvent, TxHandle, TxPlan, TxPlanEvent, TX_VOX_SECONDS,
};
use modem_worker_base::{EventSink, EventSinkExt, ptt::SharedPtt};

/// Protocol-level mode byte for the 2x family. Wire frames encode this
/// as `AppHeader.mode_code`; the RX uses it as a coarse family hint
/// and the CLI / GUI use it to seed `compute_session_id` so the same
/// payload + profile + envelope always lands on the same disk session
/// (no `--session-id` override needed for continuation bursts). Pinned
/// to 0xA5 in `frame2x` (see [[modem-2x-branch-state]] decision log).
pub const V4_MODE_CODE: u8 = 0xA5;

/// Errors surfaced by the TX worker.
#[derive(Debug)]
pub enum TxError {
    /// The modem rejected the request (unknown profile, invalid
    /// `(symbol_rate, tau)` combination, ...).
    Modem(ModemError),
    /// File I/O failed when writing a WAV.
    Wav(hound::Error),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Modem(e) => write!(f, "modem error: {e}"),
            Self::Wav(e) => write!(f, "wav write error: {e}"),
        }
    }
}

impl std::error::Error for TxError {}

impl From<ModemError> for TxError {
    fn from(e: ModemError) -> Self { Self::Modem(e) }
}
impl From<hound::Error> for TxError {
    fn from(e: hound::Error) -> Self { Self::Wav(e) }
}

/// Encode an [`EncodeRequest`] to mono 48 kHz `f32` audio samples.
///
/// Thin pass-through over [`V4Modem::encode_to_samples`]; exposed here so
/// every TX path (GUI, CLI, tests) goes through one entry point and the
/// future `tx_worker_base` factoring stays trivial.
pub fn encode_to_audio(req: &EncodeRequest<'_>) -> Result<Vec<f32>, TxError> {
    Ok(V4Modem.encode_to_samples(req)?)
}

/// Encode an [`EncodeRequest`] and write the resulting audio to a WAV
/// file at `path`. Mono, 48 kHz, 32-bit float — the format the GUI's
/// playback path reads back without conversion.
///
/// Returns the number of samples actually written.
pub fn encode_to_wav(req: &EncodeRequest<'_>, path: &Path) -> Result<usize, TxError> {
    let samples = encode_to_audio(req)?;
    let spec = WavSpec {
        channels: 1,
        sample_rate: AUDIO_RATE,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(path, spec)?;
    for s in &samples {
        writer.write_sample(*s)?;
    }
    writer.finalize()?;
    Ok(samples.len())
}

// ---------------------------------------------------------------------------
// GUI orchestration (mirror of modem-worker::tx_worker's tx_plan / spawn /
// spawn_more, but driving the V4 frame layout + V4Modem encoder).
// ---------------------------------------------------------------------------

/// FNV-1a 32-bit + xor-fold to 16 bits — same routine the V3 worker
/// and the CLI use to derive the app-header `hash_short` field.
fn fnv_short(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

/// Map a source-file extension to the protocol-level mime byte. Same
/// mapping as `modem-worker::tx_worker::infer_mime` (and the CLI's
/// `infer_mime`) so the disk-side payload type is consistent across
/// families.
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

/// Compute the V4 transmission plan for a given payload + profile +
/// chosen RaptorQ repair percentage. Symbol-accurate via
/// [`superframe_total_symbols_v4`] + [`eot_frame_symbols_v4`], so the
/// GUI duration estimate matches what actually goes on air for every
/// 2x profile (including the experimental 5.6 ksym/s variants).
///
/// Unlike V3, the 2x frame builder does not require `n_initial` to be
/// even — every cycle is self-contained, so we pass `k + repair` through
/// directly without `effective_packet_count`.
pub fn tx_plan(
    payload_bytes: usize,
    mode_name: &str,
    callsign_len: usize,
    filename_len: usize,
    repair_pct: u32,
) -> Result<TxPlan, String> {
    let cfg = config_by_name_2x(mode_name)
        .ok_or_else(|| format!("unknown 2x profile '{mode_name}'"))?;
    let envelope_overhead = 2 + filename_len + 1 + callsign_len + 1;
    let wire = payload_bytes + envelope_overhead;
    let k_bytes = cfg.base.ldpc_rate.k() / 8;
    let k_source = k_from_payload(wire, k_bytes) as u32;
    let n_initial = k_source + (k_source * repair_pct) / 100;

    let bits_per_cw = (k_bytes as f64) * 8.0;
    let seconds_per_cw = bits_per_cw / cfg.net_bitrate();

    let total_syms_initial =
        superframe_total_symbols_v4(&cfg, n_initial) + eot_frame_symbols_v4(&cfg);
    let duration_s_initial = total_syms_initial as f64 / cfg.base.symbol_rate;
    let duration_s_k =
        superframe_total_symbols_v4(&cfg, k_source) as f64 / cfg.base.symbol_rate;
    Ok(TxPlan {
        k_source,
        n_initial,
        duration_s_initial,
        duration_s_k,
        seconds_per_cw,
    })
}

/// Read the source file, build the wire payload (envelope-wrapped),
/// derive session_id + hash + mime, and call `V4Modem.encode_to_samples`
/// to produce the audio. Used for both the initial burst (esi_start=0)
/// and the "More" continuation burst (esi_start>0). Mirror of
/// `modem-worker::tx_worker::encode_in_process` for the 2x family.
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

    // 2x has no protocol-level ProfileIndex byte (every profile is fixed
    // config; the wire frame just carries V4_MODE_CODE). session_id is
    // still keyed on the same (mode_code, profile_index) pair the V3
    // RX expects, so we substitute V4_MODE_CODE for both — same
    // convention the CLI uses, see [[modem-2x-branch-state]].
    let session_id =
        app_header::compute_session_id(&wire_payload, V4_MODE_CODE, V4_MODE_CODE);
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
    V4Modem
        .encode_to_samples(&req)
        .map_err(|e| format!("encode: {e}"))
}

/// Initial 2x TX burst: encodes K source + repair packets so the RX has
/// enough redundancy to fountain-decode without retransmission, on a
/// loss-free channel. GUI-side mirror of
/// `modem-worker::tx_worker::spawn`.
#[allow(clippy::too_many_arguments)]
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
    tx_sink: Arc<dyn SampleSink>,
    ptt: SharedPtt,
    sink: Arc<dyn EventSink>,
    save_wav_dir: Option<PathBuf>,
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

        let wav_path = save_wav_dir
            .as_deref()
            .map(|d| build_tx_wav_path(d, &filename, 0))
            .unwrap_or_default();
        sink.emit(
            "tx_plan",
            TxPlanEvent {
                duration_s,
                total_blocks,
                wire_bytes,
                wav_path: wav_path.clone(),
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
            wav_path,
            attenuation_db,
            preemphasis_enabled,
            tx_sink,
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

/// 2x continuation burst (RaptorQ "More"): encodes `count` packets
/// starting at `esi_start`, reusing the same envelope/session/mime as
/// the initial burst. Mirror of `modem-worker::tx_worker::spawn_more`.
#[allow(clippy::too_many_arguments)]
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
    tx_sink: Arc<dyn SampleSink>,
    ptt: SharedPtt,
    sink: Arc<dyn EventSink>,
    save_wav_dir: Option<PathBuf>,
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
        let wav_path = save_wav_dir
            .as_deref()
            .map(|d| build_tx_wav_path(d, &filename, esi_start))
            .unwrap_or_default();
        run_playback(
            &device_name,
            samples,
            total_blocks,
            duration_s,
            wav_path,
            attenuation_db,
            preemphasis_enabled,
            tx_sink,
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

#[cfg(test)]
mod tests {
    use super::*;
    use modem_framing::app_header::mime;
    use tempfile::NamedTempFile;

    fn req() -> EncodeRequest<'static> {
        EncodeRequest {
            profile: "HIGH2X",
            wire_payload: b"hello modem 2x",
            session_id: 0xCAFE_BABE,
            mime_type: mime::BINARY,
            hash_short: 0xAA55,
            esi_start: 0,
            n_packets: 6,
            vox_seconds: 0.0,
        }
    }

    #[test]
    fn encode_to_audio_returns_non_empty_buffer() {
        let audio = encode_to_audio(&req()).expect("encode");
        assert!(!audio.is_empty());
        let max_abs = audio.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
        assert!(max_abs > 0.05);
        assert!(max_abs <= 1.0);
    }

    #[test]
    fn encode_to_audio_unknown_profile_fails() {
        let mut r = req();
        r.profile = "NOPE";
        match encode_to_audio(&r).unwrap_err() {
            TxError::Modem(ModemError::UnknownProfile(_)) => {}
            other => panic!("expected UnknownProfile, got {other}"),
        }
    }

    #[test]
    fn tx_plan_unknown_profile_errors() {
        match tx_plan(100, "NOPE", 6, 8, 30) {
            Ok(_) => panic!("expected error for unknown profile"),
            Err(e) => assert!(e.contains("NOPE"), "unhelpful message: {e}"),
        }
    }

    #[test]
    fn tx_plan_duration_matches_actual_audio_high2x() {
        // Plan for a 200-byte payload through HIGH2X with 30% repair.
        // Then encode the same payload through encode_in_process at the
        // planned n_initial and verify the actual audio length
        // (seconds, sans VOX) is within 0.5% of plan.duration_s_initial.
        // 0.5% guards against integer-rounding noise in the symbol-
        // count math; anything looser would mask a real bug.
        let payload_bytes = 200;
        let callsign = "HB9TOB";
        let filename = "x.bin";
        let plan = tx_plan(payload_bytes, "HIGH2X", callsign.len(), filename.len(), 30)
            .expect("plan ok");
        assert!(plan.n_initial >= plan.k_source);
        assert!(plan.duration_s_initial > 0.0);

        // Synthesise the matching audio (vox=0, plan-equivalent path).
        let payload = vec![0xA5u8; payload_bytes];
        let tmp = NamedTempFile::new().expect("tmp");
        std::fs::write(tmp.path(), &payload).expect("write payload");
        let audio = encode_in_process(
            tmp.path(),
            "HIGH2X",
            callsign,
            filename,
            0,
            plan.n_initial,
        )
        .expect("encode in process");
        // encode_in_process passes vox_seconds = TX_VOX_SECONDS (0.5s)
        // + 50ms tail silence + PRBS pre-burst + 200ms inter-frame
        // silence + 100ms post-EOT silence. Strip those for the
        // data+EOT comparison. The pre-burst contributes exactly the
        // same modulator-tail-included length as any other symbol
        // burst, so we reproduce the modulate() call here to subtract
        // it.
        let vox_overhead_s = TX_VOX_SECONDS + 0.05 + 0.1; // tone+tail+post-EOT
        let cfg = config_by_name_2x("HIGH2X").expect("HIGH2X cfg");
        let (sps, pitch) = modem_core_base::rrc::check_integer_constraints(
            AUDIO_RATE,
            cfg.base.symbol_rate,
            cfg.base.tau,
        ).expect("rrc check");
        let taps = modem_core_base::rrc::rrc_taps(
            cfg.base.beta,
            modem_core_base::types::RRC_SPAN_SYM,
            sps,
        );
        let preburst_audio = modem_core_base::modulator::modulate(
            modem_core2x::preburst::reference_symbols(),
            sps,
            pitch,
            &taps,
            cfg.base.center_freq_hz,
        );
        let preburst_s = preburst_audio.len() as f64 / AUDIO_RATE as f64;
        let actual_total_s = audio.len() as f64 / AUDIO_RATE as f64;
        // Plan already includes the 200ms inter-frame silence implicitly
        // via the EOT slot in the wire layout? No — V4 plan covers only
        // the modulated symbols (data superframe + EOT frame). The
        // 200ms silence between them and the 100ms post-EOT silence sit
        // OUTSIDE the symbol stream. Plan therefore tracks
        // (actual_total_s - vox_overhead_s - preburst_s - 0.2 [inter-frame]).
        let actual_modulated_s = actual_total_s - vox_overhead_s - preburst_s - 0.2;
        let drift = (actual_modulated_s - plan.duration_s_initial).abs()
            / plan.duration_s_initial;
        assert!(
            drift < 0.005,
            "plan {:.3}s vs actual modulated {:.3}s (drift {:.3}%)",
            plan.duration_s_initial,
            actual_modulated_s,
            drift * 100.0,
        );
    }

    #[test]
    fn encode_to_wav_writes_readable_file() {
        let tmp = NamedTempFile::new().expect("tmp");
        let path = tmp.path().to_path_buf();
        let n = encode_to_wav(&req(), &path).expect("write wav");
        assert!(n > 0);
        // Re-read the WAV and verify it has the expected header.
        let reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, AUDIO_RATE);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, SampleFormat::Float);
        // Sample count matches what encode_to_audio returned.
        let n_samples = reader.duration() as usize;
        assert_eq!(n_samples, n);
    }
}
