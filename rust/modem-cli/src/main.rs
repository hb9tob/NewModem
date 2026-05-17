use clap::{Parser, Subcommand};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use modem_core::profile::{
    self, ConstellationType, LdpcRate, ModemConfig,
};
use modem_core::types::AUDIO_RATE;
use modem_core_base::traits::EncodeRequest;
use modem_core2x::profile2x::ProfileIndex2x;
use modem_worker2x::{rx_worker2x, tx_worker2x};
use std::path::PathBuf;

/// AppHeader.mode_code byte that the V4 encoder embeds (see
/// `modem_core2x::frame2x`). The CLI uses the same constant when computing
/// session_id so the value is reproducible between Tx and TxMore.
const V4_MODE_CODE: u8 = 0xA5;

#[derive(Parser)]
#[command(
    name = "nbfm-modem",
    about = "NBFM audio modem — WAV file TX/RX (V3 legacy + V4 2x families)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Wire-format family selector for the `--family` CLI flag.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Family {
    Legacy,
    TwoX,
}

fn parse_family(s: &str) -> Family {
    match s.to_lowercase().as_str() {
        "legacy" | "v3" | "1x" => Family::Legacy,
        "2x" | "v4" => Family::TwoX,
        _ => {
            eprintln!("Unknown --family '{s}'. Expected: legacy | 2x");
            std::process::exit(1);
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Encode a file to a WAV audio signal.
    Tx {
        /// Input file to transmit
        #[arg(short, long)]
        input: PathBuf,

        /// Output WAV file
        #[arg(short, long)]
        output: PathBuf,

        /// Wire-format family: `legacy` (V3) or `2x` (V4).
        #[arg(long, default_value = "legacy")]
        family: String,

        /// Predefined profile. Legacy: MEGA, HIGH, NORMAL, ROBUST, ULTRA
        /// (and HIGH+, HIGH56, HIGH+56). 2x: NORMAL2X, HIGH2X, ROBUST2X,
        /// ULTRA2X, HIGH+2X, HIGH++2X, HIGH56_2X, HIGH+56_2X.
        #[arg(short, long, default_value = "NORMAL")]
        profile: String,

        /// Override constellation: qpsk, 8psk, 16apsk
        #[arg(long)]
        constellation: Option<String>,

        /// Override LDPC rate: 1/2, 2/3, 3/4
        #[arg(long)]
        ldpc_rate: Option<String>,

        /// Override symbol rate (Bd)
        #[arg(long)]
        rs: Option<f64>,

        /// Override RRC rolloff beta
        #[arg(long)]
        beta: Option<f64>,

        /// Override center frequency (Hz)
        #[arg(long)]
        fc: Option<f64>,

        /// VOX preamble duration (seconds, 0 to disable)
        #[arg(long, default_value = "0.5")]
        vox: f64,

        /// 32-bit session identifier (hex, random if omitted)
        #[arg(long)]
        session_id: Option<String>,

        /// Original filename to embed in the payload envelope (default = basename
        /// of --input, max 64 UTF-8 bytes)
        #[arg(long)]
        filename: Option<String>,

        /// Operator callsign (QRZ) to embed in the payload envelope (required,
        /// max 10 ASCII bytes). Example: HB9TOB.
        #[arg(long)]
        callsign: Option<String>,

        /// RaptorQ repair percentage on top of K for the initial burst
        /// (0..=200). 0 emits exactly K codewords (zero margin, RX must
        /// recover every packet). Default 30.
        #[arg(long, default_value = "30")]
        repair_pct: u32,
    },

    /// Emit an additional burst of RaptorQ repair packets for an already-sent
    /// file, using the same session_id and continuing ESIs after the previous
    /// burst. The operator uses this when an RX vocally requests more packets.
    TxMore {
        /// Input file : must be the exact same bytes that were sent initially.
        #[arg(short, long)]
        input: PathBuf,

        /// Output WAV file
        #[arg(short, long)]
        output: PathBuf,

        /// Wire-format family (must match the initial burst): `legacy` or `2x`.
        #[arg(long, default_value = "legacy")]
        family: String,

        /// Profile (must match initial burst)
        #[arg(short, long, default_value = "NORMAL")]
        profile: String,

        /// Override constellation, LDPC, symbol rate, β, fc (as in `tx`).
        #[arg(long)]
        constellation: Option<String>,
        #[arg(long)]
        ldpc_rate: Option<String>,
        #[arg(long)]
        rs: Option<f64>,
        #[arg(long)]
        beta: Option<f64>,
        #[arg(long)]
        fc: Option<f64>,

        /// VOX preamble duration (seconds, 0 to disable)
        #[arg(long, default_value = "0.5")]
        vox: f64,

        /// Session ID (hex). If omitted, computed deterministically from the
        /// file contents + profile, same as `tx` would — this is what lets
        /// the RX tie this burst to the earlier one.
        #[arg(long)]
        session_id: Option<String>,

        /// ESI of the first packet to emit. Typically = (esi_max already
        /// sent) + 1. Reported by the GUI state or the caller.
        #[arg(long)]
        esi_start: u32,

        /// Percentage of K (source-symbol count) to emit in this burst.
        /// e.g. --pct 20 with K = 48 → 9 packets (one LDPC codeword each).
        /// Ignored when `--count` is provided.
        #[arg(long, default_value = "20")]
        pct: u32,

        /// Exact number of additional packets to emit. When set, overrides
        /// `--pct`. Useful when the RX reports a specific shortage ("I'm
        /// missing 5 blocks") — no more thinking in percentages.
        #[arg(long)]
        count: Option<u32>,

        #[arg(long)]
        filename: Option<String>,
        #[arg(long)]
        callsign: Option<String>,
    },

    /// Decode a WAV audio signal to a file.
    Rx {
        /// Input WAV file
        #[arg(short, long)]
        input: PathBuf,

        /// Output file for decoded data
        #[arg(short, long)]
        output: PathBuf,

        /// Wire-format family (must match TX): `legacy` (V3) or `2x` (V4).
        #[arg(long, default_value = "legacy")]
        family: String,

        /// Profile (must match TX). See `tx --help` for the per-family list.
        #[arg(short, long, default_value = "NORMAL")]
        profile: String,

        /// Override LDPC rate used at TX
        #[arg(long)]
        ldpc_rate: Option<String>,

        /// Override symbol rate
        #[arg(long)]
        rs: Option<f64>,

        /// Override center frequency
        #[arg(long)]
        fc: Option<f64>,
    },

    /// Inspect a WAV file (samples, peak, RMS, duration)
    Info {
        /// Input WAV file
        #[arg(short, long)]
        input: PathBuf,
    },

    /// Analyse a recorded sounder capture against its TX-side
    /// schedule. Locates the sync chirp in the recording, applies
    /// every probe's measurer at its known offset, writes a
    /// channel-signature JSON next to the capture.
    ///
    /// Pure file-IO front-end on top of
    /// `modem_worker_base::sounder::analyze_capture`.
    SoundingAnalyze {
        /// Recorded capture WAV (48 kHz mono, generated by the GUI's
        /// raw-capture button or by any external recorder).
        #[arg(long)]
        capture: PathBuf,

        /// Probe schedule JSON written by the TX side via
        /// `build_probe_schedule`.
        #[arg(long)]
        schedule: PathBuf,

        /// Where to write the channel-signature JSON.
        #[arg(long)]
        signature: PathBuf,

        /// TX operator callsign (recorded into the signature metadata).
        #[arg(long, default_value = "")]
        tx_callsign: String,

        /// RX operator callsign.
        #[arg(long, default_value = "")]
        rx_callsign: String,

        /// Free-form equipment string ("FTX-1 + soundcard", ...).
        #[arg(long, default_value = "")]
        equipment: String,

        /// Free-form notes recorded into the signature.
        #[arg(long, default_value = "")]
        notes: String,

        /// Channel family the operator was sounding: `fm`, `qo100` or
        /// `ssb_hf`. Phase 1 only meaningfully exercises `fm`; the
        /// other two are reserved for the upcoming QO-100 and HF
        /// extensions.
        #[arg(long, default_value = "fm")]
        family: String,

        /// Cross-correlation peak threshold factor (peak / RMS).
        /// 6.0 keeps the false-positive rate near 10⁻⁹ on AWGN.
        #[arg(long, default_value_t = 6.0)]
        sync_threshold: f32,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Tx {
            input,
            output,
            family,
            profile,
            constellation,
            ldpc_rate,
            rs,
            beta,
            fc,
            vox,
            session_id,
            filename,
            callsign,
            repair_pct,
        } => {
            if parse_family(&family) == Family::TwoX {
                reject_legacy_overrides_2x(&constellation, &ldpc_rate, &rs, &beta, &fc);
                run_tx_2x(
                    &input,
                    &output,
                    &profile,
                    vox,
                    session_id.as_deref(),
                    filename.as_deref(),
                    callsign.as_deref(),
                    repair_pct,
                );
                return;
            }
            let mut config = parse_profile(&profile);

            if let Some(c) = constellation {
                config.constellation = parse_constellation(&c);
            }
            if let Some(r) = ldpc_rate {
                config.ldpc_rate = parse_ldpc_rate(&r);
            }
            if let Some(r) = rs {
                config.symbol_rate = r;
            }
            if let Some(b) = beta {
                config.beta = b;
            }
            if let Some(f) = fc {
                config.center_freq_hz = f;
            }

            let data = std::fs::read(&input).unwrap_or_else(|e| {
                eprintln!("Error reading {}: {e}", input.display());
                std::process::exit(1);
            });

            eprintln!(
                "TX v3: {} bytes, profile={}, constellation={:?}, LDPC={:?}, Rs={} Bd, beta={}, fc={} Hz",
                data.len(),
                profile,
                config.constellation,
                config.ldpc_rate,
                config.symbol_rate,
                config.beta,
                config.center_freq_hz
            );

            let fname = filename
                .clone()
                .unwrap_or_else(|| infer_filename(&input));
            let qrz = callsign.clone().unwrap_or_else(|| {
                eprintln!("TX error: --callsign is required (e.g. HB9TOB)");
                std::process::exit(1);
            });
            let envelope = modem_framing::payload_envelope::PayloadEnvelope::new(
                &fname,
                &qrz,
                data.clone(),
            )
            .unwrap_or_else(|| {
                eprintln!(
                    "TX error: filename (len {}) / callsign (len {}) exceed size limits or contain NUL",
                    fname.len(),
                    qrz.len()
                );
                std::process::exit(1);
            });
            let wire_payload = envelope.encode();

            let profile_index = profile_index_of(&profile);
            let sid = session_id
                .as_deref()
                .map(parse_explicit_session_id)
                .unwrap_or_else(|| {
                    modem_framing::app_header::compute_session_id(
                        &wire_payload,
                        config.mode_code(),
                        profile_index,
                    )
                });
            let hash = content_hash_short(&wire_payload);
            let mime = infer_mime(&input);
            eprintln!(
                "TX v3: session_id=0x{:08X}, callsign={}, filename={}, mime=0x{:02X}, hash=0x{:04X}",
                sid, qrz, fname, mime, hash
            );

            let k_bytes_for_plan =
                modem_core::ldpc::encoder::LdpcEncoder::new(config.ldpc_rate).k() / 8;
            let k_src =
                modem_framing::raptorq_codec::k_from_payload(wire_payload.len(), k_bytes_for_plan)
                    as u32;
            // n_total rounded so the last data segment is always complete;
            // otherwise the RX loses that final CW (cf. effective_packet_count).
            let n_total = modem_core::frame::effective_packet_count(
                k_src + (k_src * repair_pct) / 100,
            );
            eprintln!(
                "TX v3: K={k_src}, repair_pct={repair_pct}, n_total={n_total} packets"
            );
            let symbols = modem_core::frame::build_superframe_v3_range(
                &wire_payload, &config, sid, mime, hash, 0, n_total,
            );

            let (sps, pitch) = modem_core::rrc::check_integer_constraints(
                AUDIO_RATE,
                config.symbol_rate,
                config.tau,
            )
            .expect("invalid profile");
            let taps =
                modem_core::rrc::rrc_taps(config.beta, modem_core::types::RRC_SPAN_SYM, sps);
            let mut modulated = modem_core::modulator::modulate(
                &symbols,
                sps,
                pitch,
                &taps,
                config.center_freq_hz,
            );
            let eot_symbols = modem_core::frame::build_eot_frame(&config, sid);
            let mut eot_modulated = modem_core::modulator::modulate(
                &eot_symbols,
                sps,
                pitch,
                &taps,
                config.center_freq_hz,
            );

            let samples = if vox > 0.0 {
                let mut out = Vec::new();
                out.extend_from_slice(&modem_core::modulator::tone(
                    config.center_freq_hz,
                    vox,
                    0.5,
                ));
                out.extend_from_slice(&modem_core::modulator::silence(0.05));
                out.append(&mut modulated);
                out.extend_from_slice(&modem_core::modulator::silence(0.2));
                out.append(&mut eot_modulated);
                out.extend_from_slice(&modem_core::modulator::silence(0.1));
                out
            } else {
                let mut out = modulated;
                out.extend_from_slice(&modem_core::modulator::silence(0.2));
                out.append(&mut eot_modulated);
                out
            };

            let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
            eprintln!("Generated {} samples ({:.2}s)", samples.len(), duration_s);

            write_wav(&output, &samples);
            eprintln!("Written to {}", output.display());
        }

        Commands::TxMore {
            input,
            output,
            family,
            profile,
            constellation,
            ldpc_rate,
            rs,
            beta,
            fc,
            vox,
            session_id,
            esi_start,
            pct,
            count,
            filename,
            callsign,
        } => {
            if parse_family(&family) == Family::TwoX {
                reject_legacy_overrides_2x(&constellation, &ldpc_rate, &rs, &beta, &fc);
                run_tx_more_2x(
                    &input,
                    &output,
                    &profile,
                    vox,
                    session_id.as_deref(),
                    filename.as_deref(),
                    callsign.as_deref(),
                    esi_start,
                    pct,
                    count,
                );
                return;
            }
            let mut config = parse_profile(&profile);
            if let Some(c) = constellation {
                config.constellation = parse_constellation(&c);
            }
            if let Some(r) = ldpc_rate {
                config.ldpc_rate = parse_ldpc_rate(&r);
            }
            if let Some(r) = rs {
                config.symbol_rate = r;
            }
            if let Some(b) = beta {
                config.beta = b;
            }
            if let Some(f) = fc {
                config.center_freq_hz = f;
            }
            let data = std::fs::read(&input).unwrap_or_else(|e| {
                eprintln!("Error reading {}: {e}", input.display());
                std::process::exit(1);
            });
            let fname = filename.unwrap_or_else(|| infer_filename(&input));
            let qrz = callsign.unwrap_or_else(|| {
                eprintln!("tx-more error: --callsign required");
                std::process::exit(1);
            });
            let envelope = modem_framing::payload_envelope::PayloadEnvelope::new(
                &fname, &qrz, data.clone(),
            )
            .unwrap_or_else(|| {
                eprintln!("tx-more error: envelope too large");
                std::process::exit(1);
            });
            let wire_payload = envelope.encode();

            let profile_index = profile_index_of(&profile);
            let sid = session_id
                .as_deref()
                .map(parse_explicit_session_id)
                .unwrap_or_else(|| {
                    modem_framing::app_header::compute_session_id(
                        &wire_payload,
                        config.mode_code(),
                        profile_index,
                    )
                });
            let hash = content_hash_short(&wire_payload);
            let mime = infer_mime(&input);

            // K = source-symbol count. Packet count = explicit `count` if
            // provided, else derived from `pct`. The explicit path is what the
            // GUI uses now (user picks "+N blocs" directly); the percentage
            // path is kept for backwards-compat and CLI scripting.
            let k_bytes = modem_core::profile::LdpcRate::k(config.ldpc_rate) / 8;
            let k =
                modem_framing::raptorq_codec::k_from_payload(wire_payload.len(), k_bytes) as u32;
            let n_packets_raw = match count {
                Some(c) => c,
                None => (k * pct) / 100,
            };
            if n_packets_raw == 0 {
                eprintln!("tx-more: empty burst (K={k}, pct={pct}%, count={count:?})");
                std::process::exit(1);
            }
            // See effective_packet_count: rounded so a partial segment is
            // never left dangling at the end of a burst.
            let n_packets = modem_core::frame::effective_packet_count(n_packets_raw);
            eprintln!(
                "tx-more: session=0x{sid:08X}, K={k}, esi_start={esi_start}, count={n_packets}"
            );

            let symbols = modem_core::frame::build_superframe_v3_range(
                &wire_payload,
                &config,
                sid,
                mime,
                hash,
                esi_start,
                n_packets,
            );
            let (sps, pitch) = modem_core::rrc::check_integer_constraints(
                AUDIO_RATE,
                config.symbol_rate,
                config.tau,
            )
            .expect("invalid profile");
            let taps =
                modem_core::rrc::rrc_taps(config.beta, modem_core::types::RRC_SPAN_SYM, sps);
            let mut modulated = modem_core::modulator::modulate(
                &symbols,
                sps,
                pitch,
                &taps,
                config.center_freq_hz,
            );
            let eot_symbols = modem_core::frame::build_eot_frame(&config, sid);
            let mut eot_modulated = modem_core::modulator::modulate(
                &eot_symbols,
                sps,
                pitch,
                &taps,
                config.center_freq_hz,
            );
            let samples = if vox > 0.0 {
                let mut out = Vec::new();
                out.extend_from_slice(&modem_core::modulator::tone(
                    config.center_freq_hz,
                    vox,
                    0.5,
                ));
                out.extend_from_slice(&modem_core::modulator::silence(0.05));
                out.append(&mut modulated);
                out.extend_from_slice(&modem_core::modulator::silence(0.2));
                out.append(&mut eot_modulated);
                out.extend_from_slice(&modem_core::modulator::silence(0.1));
                out
            } else {
                let mut out = modulated;
                out.extend_from_slice(&modem_core::modulator::silence(0.2));
                out.append(&mut eot_modulated);
                out
            };
            eprintln!(
                "Generated {} samples ({:.2}s)",
                samples.len(),
                samples.len() as f64 / AUDIO_RATE as f64
            );
            write_wav(&output, &samples);
            eprintln!("Written to {}", output.display());
        }

        Commands::Rx {
            input,
            output,
            family,
            profile,
            ldpc_rate,
            rs,
            fc,
        } => {
            if parse_family(&family) == Family::TwoX {
                reject_legacy_overrides_2x(&None, &ldpc_rate, &rs, &None, &fc);
                run_rx_2x(&input, &output, &profile);
                return;
            }
            let mut config = parse_profile(&profile);
            if let Some(r) = ldpc_rate {
                config.ldpc_rate = parse_ldpc_rate(&r);
            }
            if let Some(r) = rs {
                config.symbol_rate = r;
            }
            if let Some(f) = fc {
                config.center_freq_hz = f;
            }

            let samples = read_wav(&input);
            let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
            eprintln!(
                "RX v3: {} samples ({:.2}s) from {}",
                samples.len(),
                duration_s,
                input.display()
            );

            match modem_core::rx_v2::rx_v3(&samples, &config) {
                Some(result) => {
                    eprintln!(
                        "Decoded: {} bytes, {}/{} LDPC blocks converged, {} segments, {} lost, sigma²={:.4}",
                        result.data.len(),
                        result.converged_blocks,
                        result.total_blocks,
                        result.segments_decoded,
                        result.segments_lost,
                        result.sigma2
                    );
                    if let Some(ref hdr) = result.header {
                        let profile_name = modem_core::profile::ProfileIndex::from_u8(
                            hdr.profile_index,
                        )
                        .map(|p| p.name())
                        .unwrap_or("UNKNOWN");
                        eprintln!(
                            "Protocol header: version={}, mode=0x{:02X}, profile={} ({}), payload_len={}",
                            hdr.version,
                            hdr.mode_code,
                            profile_name,
                            hdr.profile_index,
                            hdr.payload_length
                        );
                    }
                    if let Some(ref ah) = result.app_header {
                        eprintln!(
                            "App header: session_id=0x{:08X}, file_size={}, K={}, T={}, mime=0x{:02X}, hash=0x{:04X}",
                            ah.session_id, ah.file_size, ah.k_symbols, ah.t_bytes,
                            ah.mime_type, ah.hash_short
                        );
                    }

                    let envelope = modem_framing::payload_envelope::PayloadEnvelope::decode_or_fallback(
                        &result.data,
                    );
                    if envelope.version == 0 {
                        eprintln!("Payload envelope: none (writing raw content)");
                    } else {
                        eprintln!(
                            "Payload envelope: v{}, from={}, filename={}, content_size={} B",
                            envelope.version,
                            envelope.callsign,
                            envelope.filename,
                            envelope.content.len()
                        );
                    }

                    let to_write = if envelope.version == 0 {
                        &result.data
                    } else {
                        &envelope.content
                    };
                    std::fs::write(&output, to_write).unwrap_or_else(|e| {
                        eprintln!("Error writing {}: {e}", output.display());
                        std::process::exit(1);
                    });
                    eprintln!("Written {} bytes to {}", to_write.len(), output.display());
                }
                None => {
                    eprintln!(
                        "RX failed: no preamble found or signal too short"
                    );
                    std::process::exit(1);
                }
            }
        }

        Commands::Info { input } => {
            let samples = read_wav(&input);
            let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
            println!("File: {}", input.display());
            println!("Samples: {} ({:.2}s at {} Hz)", samples.len(), duration_s, AUDIO_RATE);

            let peak = samples.iter().map(|&s| s.abs()).fold(0.0f32, f32::max);
            let rms = (samples.iter().map(|&s| s as f64 * s as f64).sum::<f64>()
                / samples.len() as f64)
                .sqrt();
            println!("Peak: {:.4}, RMS: {:.4}", peak, rms);
        }

        Commands::SoundingAnalyze {
            capture,
            schedule,
            signature,
            tx_callsign,
            rx_callsign,
            equipment,
            notes,
            family,
            sync_threshold,
        } => {
            run_sounding_analyze(
                &capture,
                &schedule,
                &signature,
                &tx_callsign,
                &rx_callsign,
                &equipment,
                &notes,
                &family,
                sync_threshold,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sounding_analyze(
    capture: &PathBuf,
    schedule: &PathBuf,
    signature: &PathBuf,
    tx_callsign: &str,
    rx_callsign: &str,
    equipment: &str,
    notes: &str,
    family: &str,
    sync_threshold: f32,
) {
    use modem_worker_base::sounder::{
        analyze_capture, ChannelFamily, ProbeSchedule, SoundingMetadata,
    };
    // Load schedule JSON.
    let sched_bytes = std::fs::read(schedule).unwrap_or_else(|e| {
        eprintln!("read schedule {}: {e}", schedule.display());
        std::process::exit(1);
    });
    let sched: ProbeSchedule = serde_json::from_slice(&sched_bytes)
        .unwrap_or_else(|e| {
            eprintln!("parse schedule {}: {e}", schedule.display());
            std::process::exit(1);
        });
    if sched.sample_rate != AUDIO_RATE {
        eprintln!(
            "schedule sample_rate {} != CLI AUDIO_RATE {}",
            sched.sample_rate, AUDIO_RATE,
        );
        std::process::exit(1);
    }
    // Load capture WAV.
    let capture_audio = read_wav(capture);
    if capture_audio.is_empty() {
        eprintln!("capture {} is empty", capture.display());
        std::process::exit(1);
    }
    // Resolve family.
    let fam = match family.to_lowercase().as_str() {
        "fm" => ChannelFamily::Fm,
        "qo100" | "qo-100" => ChannelFamily::Qo100,
        "ssb_hf" | "ssb-hf" | "ssbhf" => ChannelFamily::SsbHf,
        other => {
            eprintln!("Unknown --family '{other}'. Expected: fm | qo100 | ssb_hf");
            std::process::exit(1);
        }
    };
    let metadata = SoundingMetadata {
        tx_callsign: tx_callsign.into(),
        rx_callsign: rx_callsign.into(),
        equipment: equipment.into(),
        notes: notes.into(),
        ts_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    // Run the analyser.
    let sig = match analyze_capture(
        &capture_audio,
        &sched,
        fam,
        metadata,
        sync_threshold,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("analyse failed: {e}");
            std::process::exit(2);
        }
    };
    // Write signature JSON.
    let serialised = serde_json::to_string_pretty(&sig).unwrap_or_else(|e| {
        eprintln!("serialise signature: {e}");
        std::process::exit(1);
    });
    if let Some(parent) = signature.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(signature, &serialised).unwrap_or_else(|e| {
        eprintln!("write signature {}: {e}", signature.display());
        std::process::exit(1);
    });
    // Print a short summary so the operator gets immediate feedback.
    println!("Sounding signature written: {}", signature.display());
    println!("  anchor sample      : {}", sig.capture_anchor_sample);
    println!("  channel family     : {:?}", sig.channel_family);
    println!("  measurements       : {}", sig.measurements.len());
    println!("  derived:");
    println!("    SNR (mean tones) : {:.2} dB", sig.derived.snr_est_db);
    if sig.derived.ip3_dbfs.is_finite() {
        println!("    IP3 (output)     : {:.2} dBFS", sig.derived.ip3_dbfs);
    }
    if sig.derived.p1db_dbfs.is_finite() {
        println!("    P1dB (input)     : {:.2} dBFS", sig.derived.p1db_dbfs);
    }
    if sig.derived.sweet_spot_dbfs.is_finite() {
        println!("    sweet spot       : {:.2} dBFS", sig.derived.sweet_spot_dbfs);
    }
    if sig.derived.bw_3db_hz.1 > 0.0 {
        println!(
            "    BW -3 dB         : {:.0}-{:.0} Hz",
            sig.derived.bw_3db_hz.0, sig.derived.bw_3db_hz.1
        );
    }
    if sig.derived.group_delay_peak_us > 0.0 {
        println!(
            "    group delay peak : {:.0} µs",
            sig.derived.group_delay_peak_us
        );
    }
    if sig.derived.noise_floor_dbfs.is_finite() {
        println!(
            "    noise floor      : {:.2} dBFS",
            sig.derived.noise_floor_dbfs
        );
    }
    if sig.derived.delay_spread_50_us.is_finite() {
        println!(
            "    delay spread 50% : {:.0} µs",
            sig.derived.delay_spread_50_us
        );
    }
    if sig.derived.delay_spread_90_us.is_finite() {
        println!(
            "    delay spread 90% : {:.0} µs",
            sig.derived.delay_spread_90_us
        );
    }
    if sig.derived.strongest_echo_dbc.is_finite() {
        println!(
            "    strongest echo   : {:.1} dBc",
            sig.derived.strongest_echo_dbc
        );
    }
    if !sig.verdict.message.is_empty() {
        println!("  verdict:");
        println!("    {}", sig.verdict.message);
    }
}

fn parse_explicit_session_id(s: &str) -> u32 {
    let cleaned = s.trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(cleaned, 16).unwrap_or_else(|_| {
        eprintln!("Invalid session_id '{s}' (expected hex, e.g. 'DEADBEEF')");
        std::process::exit(1);
    })
}

fn profile_index_of(name: &str) -> u8 {
    match name.to_uppercase().as_str() {
        "MEGA" => modem_core::profile::ProfileIndex::Mega as u8,
        "HIGH" => modem_core::profile::ProfileIndex::High as u8,
        "NORMAL" => modem_core::profile::ProfileIndex::Normal as u8,
        "ROBUST" => modem_core::profile::ProfileIndex::Robust as u8,
        "ULTRA" => modem_core::profile::ProfileIndex::Ultra as u8,
        // EXPERIMENTAL - not in RX auto-detection; the peer must force the
        // mode to receive them.
        "HIGH+" | "HIGHPLUS" => modem_core::profile::ProfileIndex::HighPlus as u8,
        "FAST" => modem_core::profile::ProfileIndex::Fast as u8,
        "HIGH++" | "HIGHPLUSPLUS" => modem_core::profile::ProfileIndex::HighPlusPlus as u8,
        "HIGH56" | "HIGH-56" => modem_core::profile::ProfileIndex::HighFiveSix as u8,
        "HIGH+56" | "HIGHPLUS56" => modem_core::profile::ProfileIndex::HighPlusFiveSix as u8,
        _ => 0xFF,
    }
}

fn content_hash_short(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

fn infer_filename(path: &PathBuf) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| {
            if s.len() > modem_framing::payload_envelope::MAX_FILENAME_BYTES {
                let max = modem_framing::payload_envelope::MAX_FILENAME_BYTES;
                let (stem, ext) = s.rfind('.').map(|i| s.split_at(i)).unwrap_or((s, ""));
                let keep = max.saturating_sub(ext.len());
                format!("{}{}", &stem[..keep.min(stem.len())], ext)
            } else {
                s.to_string()
            }
        })
        .unwrap_or_else(|| "unknown.bin".to_string())
}

fn infer_mime(path: &PathBuf) -> u8 {
    use modem_framing::app_header::mime;
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

fn parse_profile(name: &str) -> ModemConfig {
    profile::config_by_name(name).unwrap_or_else(|| {
        eprintln!(
            "Unknown profile '{}'. Stable: ULTRA, ROBUST, NORMAL, HIGH, HIGH+, HIGH56. \
             Experimental (forced-mode only): MEGA, FAST, HIGH++, HIGH+56",
            name
        );
        std::process::exit(1);
    })
}

// --- 2x (V4) helpers -------------------------------------------------------

fn reject_legacy_overrides_2x(
    constellation: &Option<String>,
    ldpc_rate: &Option<String>,
    rs: &Option<f64>,
    beta: &Option<f64>,
    fc: &Option<f64>,
) {
    let mut bad: Vec<&'static str> = Vec::new();
    if constellation.is_some() { bad.push("--constellation"); }
    if ldpc_rate.is_some() { bad.push("--ldpc-rate"); }
    if rs.is_some() { bad.push("--rs"); }
    if beta.is_some() { bad.push("--beta"); }
    if fc.is_some() { bad.push("--fc"); }
    if !bad.is_empty() {
        eprintln!(
            "--family 2x is incompatible with DSP overrides ({}). 2x profiles \
             are fixed — pick a different --profile instead.",
            bad.join(", ")
        );
        std::process::exit(1);
    }
}

fn parse_profile_2x(name: &str) -> ProfileIndex2x {
    ProfileIndex2x::from_name(name).unwrap_or_else(|| {
        eprintln!(
            "Unknown 2x profile '{}'. Available: ULTRA2X, ROBUST2X, NORMAL2X, \
             HIGH2X, HIGH+2X, HIGH++2X, HIGH56_2X, HIGH+56_2X.",
            name
        );
        std::process::exit(1);
    })
}

/// Encode TX inputs that are family-agnostic (envelope, callsign, file,
/// session_id, hash, mime). Used by both `run_tx_2x` and `run_tx_more_2x`
/// so the request-construction logic stays identical.
struct Tx2xPrep {
    wire_payload: Vec<u8>,
    session_id: u32,
    mime: u8,
    hash: u16,
}

fn prepare_tx_2x(
    input: &PathBuf,
    profile_index: ProfileIndex2x,
    explicit_session_id: Option<&str>,
    filename: Option<&str>,
    callsign: Option<&str>,
) -> Tx2xPrep {
    let data = std::fs::read(input).unwrap_or_else(|e| {
        eprintln!("Error reading {}: {e}", input.display());
        std::process::exit(1);
    });
    let fname = filename
        .map(str::to_string)
        .unwrap_or_else(|| infer_filename(input));
    let qrz = callsign.map(str::to_string).unwrap_or_else(|| {
        eprintln!("TX error: --callsign is required (e.g. HB9TOB)");
        std::process::exit(1);
    });
    let envelope = modem_framing::payload_envelope::PayloadEnvelope::new(
        &fname, &qrz, data.clone(),
    )
    .unwrap_or_else(|| {
        eprintln!(
            "TX error: filename (len {}) / callsign (len {}) exceed size limits or contain NUL",
            fname.len(),
            qrz.len()
        );
        std::process::exit(1);
    });
    let wire_payload = envelope.encode();
    let sid = explicit_session_id
        .map(parse_explicit_session_id)
        .unwrap_or_else(|| {
            modem_framing::app_header::compute_session_id(
                &wire_payload,
                V4_MODE_CODE,
                profile_index.as_u8(),
            )
        });
    let hash = content_hash_short(&wire_payload);
    let mime = infer_mime(input);
    eprintln!(
        "TX 2x: session_id=0x{:08X}, callsign={}, filename={}, mime=0x{:02X}, hash=0x{:04X}",
        sid, qrz, fname, mime, hash
    );
    Tx2xPrep { wire_payload, session_id: sid, mime, hash }
}

fn run_tx_2x(
    input: &PathBuf,
    output: &PathBuf,
    profile: &str,
    vox: f64,
    explicit_session_id: Option<&str>,
    filename: Option<&str>,
    callsign: Option<&str>,
    repair_pct: u32,
) {
    let pi = parse_profile_2x(profile);
    let cfg = pi.to_config();
    let prep = prepare_tx_2x(input, pi, explicit_session_id, filename, callsign);

    // K = source-symbol count derived from the wire payload + the
    // profile's LDPC `k`. n_total = K + repair. V4 supports partial
    // cycles natively (`build_superframe_v4_range`), so we don't need
    // V3's `effective_packet_count` rounding here.
    let k_bytes = modem_core_base::profile_types::LdpcRate::k(cfg.base.ldpc_rate) / 8;
    let k_src =
        modem_framing::raptorq_codec::k_from_payload(prep.wire_payload.len(), k_bytes) as u32;
    let n_total = (k_src + (k_src * repair_pct) / 100).max(1);
    eprintln!(
        "TX 2x: profile={} ({}), K={k_src}, repair_pct={repair_pct}, n_total={n_total} packets",
        pi.name(),
        pi.as_u8()
    );

    let req = EncodeRequest {
        profile: pi.name(),
        wire_payload: &prep.wire_payload,
        session_id: prep.session_id,
        mime_type: prep.mime,
        hash_short: prep.hash,
        esi_start: 0,
        n_packets: n_total,
        vox_seconds: vox,
    };
    let n_samples = tx_worker2x::encode_to_wav(&req, output).unwrap_or_else(|e| {
        eprintln!("TX 2x error: {e}");
        std::process::exit(1);
    });
    let duration_s = n_samples as f64 / AUDIO_RATE as f64;
    eprintln!(
        "Generated {} samples ({:.2}s), written to {}",
        n_samples,
        duration_s,
        output.display()
    );
}

fn run_tx_more_2x(
    input: &PathBuf,
    output: &PathBuf,
    profile: &str,
    vox: f64,
    explicit_session_id: Option<&str>,
    filename: Option<&str>,
    callsign: Option<&str>,
    esi_start: u32,
    pct: u32,
    count: Option<u32>,
) {
    let pi = parse_profile_2x(profile);
    let cfg = pi.to_config();
    let prep = prepare_tx_2x(input, pi, explicit_session_id, filename, callsign);

    let k_bytes = modem_core_base::profile_types::LdpcRate::k(cfg.base.ldpc_rate) / 8;
    let k =
        modem_framing::raptorq_codec::k_from_payload(prep.wire_payload.len(), k_bytes) as u32;
    let n_packets = match count {
        Some(c) => c,
        None => (k * pct) / 100,
    };
    if n_packets == 0 {
        eprintln!("tx-more 2x: empty burst (K={k}, pct={pct}%, count={count:?})");
        std::process::exit(1);
    }
    eprintln!(
        "tx-more 2x: profile={}, K={k}, esi_start={esi_start}, count={n_packets}",
        pi.name()
    );

    let req = EncodeRequest {
        profile: pi.name(),
        wire_payload: &prep.wire_payload,
        session_id: prep.session_id,
        mime_type: prep.mime,
        hash_short: prep.hash,
        esi_start,
        n_packets,
        vox_seconds: vox,
    };
    let n_samples = tx_worker2x::encode_to_wav(&req, output).unwrap_or_else(|e| {
        eprintln!("tx-more 2x error: {e}");
        std::process::exit(1);
    });
    eprintln!(
        "Generated {} samples ({:.2}s), written to {}",
        n_samples,
        n_samples as f64 / AUDIO_RATE as f64,
        output.display()
    );
}

fn run_rx_2x(input: &PathBuf, output: &PathBuf, profile: &str) {
    let pi = parse_profile_2x(profile);
    let cfg = pi.to_config();
    let samples = read_wav(input);
    let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
    eprintln!(
        "RX 2x: {} samples ({:.2}s) from {}, profile={} ({})",
        samples.len(),
        duration_s,
        input.display(),
        pi.name(),
        pi.as_u8()
    );

    // Slice 2x19: drive the new Rx2xSession by chunking the WAV. The
    // session owns the full DSP state machine ; CLI just pushes
    // samples and collects events. For a one-shot WAV CLI rx the
    // chunking is purely cosmetic — we'd get the same result pushing
    // the whole buffer as a single chunk.
    let mut session = modem_core2x::rx2x_session::Rx2xSession::new(
        cfg.clone(),
        pi.name().to_string(),
    );
    let mut events: Vec<modem_core2x::rx2x_session::Rx2xEvent> = Vec::new();
    const CHUNK: usize = 24_000;
    // `RX2X_LOG_DRIFT_TICK=1` (drift validation harness) prints
    //   `[rx2x-drift-tick] t=<wav_s> drift_ppm=<x>`
    // one line per chunk. The wav-relative time tracks the *injected*
    // drift trajectory from `nbfm_channel_sim.py --drift-trace`, so the
    // two can be aligned 1:1 in the plot.
    let log_tick = std::env::var_os("RX2X_LOG_DRIFT_TICK").is_some();
    let mut audio_processed: usize = 0;
    for i in (0..samples.len()).step_by(CHUNK) {
        let end = (i + CHUNK).min(samples.len());
        events.extend(session.process_audio_chunk(&samples[i..end]));
        audio_processed += end - i;
        if log_tick {
            // cached_drift_ppm is the live LS estimate the session
            // applies to the next refresh_symbols. validated_sof_count
            // is the SOF accumulator behind the LS fit (≥ 3 = usable).
            let t = audio_processed as f64 / AUDIO_RATE as f64;
            eprintln!(
                "[rx2x-drift-tick] t={:.3} drift_ppm={:+.4} sofs={}",
                t,
                session.cached_drift_ppm(),
                session.validated_sof_count(),
            );
        }
    }
    events.extend(session.finalize());

    // Extract the final RxResult2x summary from the SessionFinalised
    // event (last one emitted).
    let result = events
        .iter()
        .rev()
        .find_map(|e| {
            if let modem_core2x::rx2x_session::Rx2xEvent::SessionFinalised { result } = e {
                Some(result.clone())
            } else {
                None
            }
        });
    let Some(result) = result else {
        eprintln!("RX 2x failed: no PLHEADER found or signal too short");
        std::process::exit(1);
    };

    eprintln!(
        "Decoded: {} bytes, {}/{} CWs converged, {} PLHEADER cycles, σ²={:.4}, EOT_seen={}",
        result.data.len(),
        result.converged_cws,
        result.total_cws,
        result.cycles,
        result.sigma2_data,
        result.eot_seen,
    );
    // Final clock drift estimate (LS slope-fit over CRC-validated
    // PLHEADER positions; emitted as NaN when fewer than 3 SOFs
    // were locked). Used by snr_sweep_2x.py's regex to plot injected
    // vs estimated drift across the validation sweep.
    let drift_str = result
        .final_drift_ppm
        .map(|p| format!("{:.2}", p))
        .unwrap_or_else(|| "NaN".to_string());
    eprintln!(
        "Drift estimate: drift_ppm={} (from {} validated SOFs)",
        drift_str,
        result.validated_sof_positions.len(),
    );
    // Channel diagnostic: pilot-residual variance decomposed along the
    // pilot direction P=(1+j)/√2 vs perpendicular. On pure AWGN
    // radial == tangential. An imbalance signals AM-AM/AM-PM distortion
    // or phase noise — see rx_v4::estimate_cw_sigma2_split docstring.
    eprintln!(
        "Channel σ² split: radial={:.4}, tangential={:.4}, ratio_R/T={:.2}",
        result.sigma2_radial,
        result.sigma2_tangential,
        result.sigma2_radial / result.sigma2_tangential.max(1e-12),
    );
    // Honest SNR from data-symbol scatter (hard-decoded to constellation).
    // Independent of every pilot-fitted estimator above — see the
    // `RxResult2x::sigma2_data_scatter` doc-comment for why this matters.
    if result.data_scatter_n > 0 {
        let snr_db = 10.0
            * (result.es_data_scatter
                / result.sigma2_data_scatter.max(1e-12))
                .log10();
        let evm_pct = (result.sigma2_data_scatter
            / result.es_data_scatter.max(1e-12))
            .sqrt()
            * 100.0;
        eprintln!(
            "Data-scatter SNR: {:.2} dB (σ²={:.5}, Es={:.4}, EVM={:.2}%, n={} syms)",
            snr_db,
            result.sigma2_data_scatter,
            result.es_data_scatter,
            evm_pct,
            result.data_scatter_n,
        );
    }
    if let Some(ref ah) = result.app_header {
        eprintln!(
            "App header: session_id=0x{:08X}, file_size={}, K={}, T={}, mime=0x{:02X}, hash=0x{:04X}",
            ah.session_id, ah.file_size, ah.k_symbols, ah.t_bytes,
            ah.mime_type, ah.hash_short
        );
    }
    if let Some(ref pls) = result.first_pls {
        let profile_name = ProfileIndex2x::from_u8(pls.profile_index)
            .map(|p| p.name())
            .unwrap_or("UNKNOWN");
        eprintln!(
            "First PLS: profile={} ({}), base_esi={}, session_id_low=0x{:02X}",
            profile_name, pls.profile_index, pls.base_esi, pls.session_id_low,
        );
    }

    let envelope =
        modem_framing::payload_envelope::PayloadEnvelope::decode_or_fallback(&result.data);
    if envelope.version == 0 {
        eprintln!("Payload envelope: none (writing raw content)");
    } else {
        eprintln!(
            "Payload envelope: v{}, from={}, filename={}, content_size={} B",
            envelope.version,
            envelope.callsign,
            envelope.filename,
            envelope.content.len()
        );
    }

    let to_write = if envelope.version == 0 {
        &result.data
    } else {
        &envelope.content
    };
    std::fs::write(output, to_write).unwrap_or_else(|e| {
        eprintln!("Error writing {}: {e}", output.display());
        std::process::exit(1);
    });
    eprintln!("Written {} bytes to {}", to_write.len(), output.display());
}

// --- Legacy (V3) helpers ---------------------------------------------------

fn parse_constellation(s: &str) -> ConstellationType {
    match s.to_lowercase().as_str() {
        "qpsk" => ConstellationType::Qpsk,
        "8psk" => ConstellationType::Psk8,
        "16apsk" | "16-apsk" | "apsk16" => ConstellationType::Apsk16,
        "32apsk" | "32-apsk" | "apsk32" => ConstellationType::Apsk32,
        "64apsk" | "64-apsk" | "apsk64" => ConstellationType::Apsk64,
        _ => {
            eprintln!(
                "Unknown constellation '{}'. Available: qpsk, 8psk, 16apsk, 32apsk, 64apsk",
                s
            );
            std::process::exit(1);
        }
    }
}

fn parse_ldpc_rate(s: &str) -> LdpcRate {
    match s {
        "1/2" | "0.5" => LdpcRate::R1_2,
        "2/3" | "0.67" => LdpcRate::R2_3,
        "3/4" | "0.75" => LdpcRate::R3_4,
        _ => {
            eprintln!("Unknown LDPC rate '{}'. Available: 1/2, 2/3, 3/4", s);
            std::process::exit(1);
        }
    }
}

fn write_wav(path: &PathBuf, samples: &[f32]) {
    let spec = WavSpec {
        channels: 1,
        sample_rate: AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec).unwrap_or_else(|e| {
        eprintln!("Error creating WAV {}: {e}", path.display());
        std::process::exit(1);
    });
    for &s in samples {
        let val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        writer.write_sample(val).unwrap();
    }
    writer.finalize().unwrap();
}

fn read_wav(path: &PathBuf) -> Vec<f32> {
    let mut reader = WavReader::open(path).unwrap_or_else(|e| {
        eprintln!("Error reading WAV {}: {e}", path.display());
        std::process::exit(1);
    });
    let spec = reader.spec();
    if spec.sample_rate != AUDIO_RATE {
        eprintln!(
            "Warning: WAV sample rate {} != expected {}",
            spec.sample_rate, AUDIO_RATE
        );
    }

    match spec.sample_format {
        SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap() as f32 / max_val)
                .collect()
        }
        SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    }
}
