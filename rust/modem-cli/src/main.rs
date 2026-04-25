use clap::{Parser, Subcommand};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use modem_core::profile::{
    self, ConstellationType, LdpcRate, ModemConfig,
};
use modem_core::types::AUDIO_RATE;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nbfm-modem", about = "NBFM audio modem — WAV file TX/RX (V3 only)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Encode a file to a WAV audio signal (V3 frame format).
    Tx {
        /// Input file to transmit
        #[arg(short, long)]
        input: PathBuf,

        /// Output WAV file
        #[arg(short, long)]
        output: PathBuf,

        /// Predefined profile: MEGA, HIGH, NORMAL, ROBUST, ULTRA
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

    /// Decode a WAV audio signal to a file (V3 frame format).
    Rx {
        /// Input WAV file
        #[arg(short, long)]
        input: PathBuf,

        /// Output file for decoded data
        #[arg(short, long)]
        output: PathBuf,

        /// Profile (must match TX): MEGA, HIGH, NORMAL, ROBUST, ULTRA
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
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Tx {
            input,
            output,
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
            let envelope = modem_core::payload_envelope::PayloadEnvelope::new(
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
                    modem_core::app_header::compute_session_id(
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
                modem_core::raptorq_codec::k_from_payload(wire_payload.len(), k_bytes_for_plan)
                    as u32;
            let n_total = k_src + (k_src * repair_pct) / 100;
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
            let envelope = modem_core::payload_envelope::PayloadEnvelope::new(
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
                    modem_core::app_header::compute_session_id(
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
                modem_core::raptorq_codec::k_from_payload(wire_payload.len(), k_bytes) as u32;
            let n_packets = match count {
                Some(c) => c,
                None => (k * pct) / 100,
            };
            if n_packets == 0 {
                eprintln!("tx-more: empty burst (K={k}, pct={pct}%, count={count:?})");
                std::process::exit(1);
            }
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
            profile,
            ldpc_rate,
            rs,
            fc,
        } => {
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

                    let envelope = modem_core::payload_envelope::PayloadEnvelope::decode_or_fallback(
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
            if s.len() > modem_core::payload_envelope::MAX_FILENAME_BYTES {
                let max = modem_core::payload_envelope::MAX_FILENAME_BYTES;
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
    use modem_core::app_header::mime;
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
    match name.to_uppercase().as_str() {
        "MEGA" => profile::profile_mega(),
        "HIGH" => profile::profile_high(),
        "NORMAL" => profile::profile_normal(),
        "ROBUST" => profile::profile_robust(),
        "ULTRA" => profile::profile_ultra(),
        _ => {
            eprintln!("Unknown profile '{}'. Available: MEGA, HIGH, NORMAL, ROBUST, ULTRA", name);
            std::process::exit(1);
        }
    }
}

fn parse_constellation(s: &str) -> ConstellationType {
    match s.to_lowercase().as_str() {
        "qpsk" => ConstellationType::Qpsk,
        "8psk" => ConstellationType::Psk8,
        "16apsk" | "16-apsk" | "apsk16" => ConstellationType::Apsk16,
        _ => {
            eprintln!("Unknown constellation '{}'. Available: qpsk, 8psk, 16apsk", s);
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
