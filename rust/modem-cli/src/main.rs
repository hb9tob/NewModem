use clap::{Parser, Subcommand};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use modem_core::profile::{
    self, ConstellationType, LdpcRate, ModemConfig,
};
use modem_core::types::AUDIO_RATE;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nbfm-modem", about = "NBFM audio modem — WAV file TX/RX")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Encode a file to a WAV audio signal
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

        /// Frame format version: 1 = monolithic (legacy), 2 = segmented with
        /// resync markers and periodic app headers (enables long-transmission
        /// recovery and RaptorQ framing)
        #[arg(long, default_value = "1")]
        frame_version: u8,

        /// v2 only: 32-bit session identifier (hex, random if omitted)
        #[arg(long)]
        session_id: Option<String>,

        /// v2 only: original filename to embed in the payload envelope
        /// (default = basename of --input, max 64 UTF-8 bytes)
        #[arg(long)]
        filename: Option<String>,

        /// v2 only: operator callsign (QRZ) to embed in the payload envelope
        /// (required for v2, max 10 ASCII bytes). Example: HB9TOB.
        #[arg(long)]
        callsign: Option<String>,
    },

    /// Decode a WAV audio signal to a file
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

        /// Override LDPC rate used at TX (required if TX used a non-default
        /// rate for this profile): 1/2, 2/3, 3/4
        #[arg(long)]
        ldpc_rate: Option<String>,

        /// Override symbol rate
        #[arg(long)]
        rs: Option<f64>,

        /// Override center frequency
        #[arg(long)]
        fc: Option<f64>,

        /// Frame format version: 1 = monolithic (legacy), 2 = segmented with
        /// resync markers. Default 1 for backward compat with existing WAVs.
        #[arg(long, default_value = "1")]
        frame_version: u8,
    },

    /// Inspect a WAV file (detect preamble, show parameters)
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
            frame_version,
            session_id,
            filename,
            callsign,
        } => {
            let mut config = parse_profile(&profile);

            // Apply overrides
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

            // Read input file
            let data = std::fs::read(&input).unwrap_or_else(|e| {
                eprintln!("Error reading {}: {e}", input.display());
                std::process::exit(1);
            });

            eprintln!("TX v{}: {} bytes, profile={}, constellation={:?}, LDPC={:?}, Rs={} Bd, beta={}, fc={} Hz",
                frame_version, data.len(), profile, config.constellation, config.ldpc_rate,
                config.symbol_rate, config.beta, config.center_freq_hz);

            // Generate audio
            let samples = match frame_version {
                1 => {
                    if vox > 0.0 {
                        modem_core::tx::tx_with_vox(&data, &config, vox)
                    } else {
                        modem_core::tx::tx(&data, &config)
                    }
                }
                2 => {
                    // Wrap user data in a payload envelope carrying filename +
                    // callsign so the RX GUI can identify the transmitter and
                    // the original file name even before decoding the content.
                    let fname = filename
                        .clone()
                        .unwrap_or_else(|| infer_filename(&input));
                    let qrz = callsign.clone().unwrap_or_else(|| {
                        eprintln!(
                            "TX v2 error: --callsign is required (e.g. HB9TOB)"
                        );
                        std::process::exit(1);
                    });
                    let envelope = modem_core::payload_envelope::PayloadEnvelope::new(
                        &fname, &qrz, data.clone(),
                    )
                    .unwrap_or_else(|| {
                        eprintln!(
                            "TX v2 error: filename (len {}) / callsign (len {}) exceed size limits or contain NUL",
                            fname.len(),
                            qrz.len()
                        );
                        std::process::exit(1);
                    });
                    let wire_payload = envelope.encode();

                    let sid = parse_session_id(session_id.as_deref());
                    let hash = content_hash_short(&wire_payload);
                    let mime = infer_mime(&input);
                    eprintln!(
                        "TX v2: session_id=0x{:08X}, callsign={}, filename={}, mime=0x{:02X}, hash=0x{:04X}",
                        sid, qrz, fname, mime, hash
                    );
                    let symbols = modem_core::frame::build_superframe_v2(
                        &wire_payload, &config, sid, mime, hash,
                    );
                    // Reuse TX modulation pipeline
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
                    // Prepend VOX tone + silence if requested (same shape as tx_with_vox)
                    if vox > 0.0 {
                        let mut out = Vec::new();
                        out.extend_from_slice(&modem_core::modulator::tone(
                            config.center_freq_hz,
                            vox,
                            0.5,
                        ));
                        out.extend_from_slice(&modem_core::modulator::silence(0.05));
                        out.append(&mut modulated);
                        out.extend_from_slice(&modem_core::modulator::silence(0.1));
                        out
                    } else {
                        modulated
                    }
                }
                v => {
                    eprintln!("Unsupported frame_version {v} (use 1 or 2)");
                    std::process::exit(1);
                }
            };

            let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
            eprintln!("Generated {} samples ({:.2}s)", samples.len(), duration_s);

            // Write WAV
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
            frame_version,
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

            // Read WAV
            let samples = read_wav(&input);
            let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
            eprintln!(
                "RX v{}: {} samples ({:.2}s) from {}",
                frame_version,
                samples.len(),
                duration_s,
                input.display()
            );

            match frame_version {
                1 => {
                    match modem_core::rx::rx(&samples, &config) {
                        Some(result) => {
                            eprintln!(
                                "Decoded: {} bytes, {}/{} LDPC blocks converged, sigma²={:.4}",
                                result.data.len(),
                                result.converged_blocks,
                                result.total_blocks,
                                result.sigma2
                            );
                            if let Some(ref hdr) = result.header {
                                eprintln!(
                                    "Header: version={}, mode=0x{:02X}, frame={}, payload_len={}, flags=0x{:02X}",
                                    hdr.version, hdr.mode_code, hdr.frame_counter,
                                    hdr.payload_length, hdr.flags
                                );
                            }
                            let payload_len = result
                                .header
                                .as_ref()
                                .map(|h| h.payload_length as usize)
                                .unwrap_or(result.data.len());
                            let trimmed = &result.data[..payload_len.min(result.data.len())];
                            std::fs::write(&output, trimmed).unwrap_or_else(|e| {
                                eprintln!("Error writing {}: {e}", output.display());
                                std::process::exit(1);
                            });
                            eprintln!("Written {} bytes to {}", trimmed.len(), output.display());
                        }
                        None => {
                            eprintln!("RX failed: no preamble found or signal too short");
                            std::process::exit(1);
                        }
                    }
                }
                2 => match modem_core::rx_v2::rx_v2(&samples, &config) {
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

                        // Attempt to unwrap the payload envelope (v2 TX adds one
                        // since the GUI RX phase). On legacy v2 transmissions
                        // without envelope, decode_or_fallback returns an empty
                        // envelope with the full buffer as content.
                        let envelope = modem_core::payload_envelope::PayloadEnvelope::decode_or_fallback(
                            &result.data,
                        );
                        if envelope.version == 0 {
                            eprintln!(
                                "Payload envelope: none (legacy v2, writing raw content)"
                            );
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
                            "RX v2 failed: no preamble found, wrong frame version, or signal too short"
                        );
                        std::process::exit(1);
                    }
                },
                v => {
                    eprintln!("Unsupported frame_version {v} (use 1 or 2)");
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

            // Try to detect preamble with each profile
            for (name, config) in [
                ("MEGA", profile::profile_mega()),
                ("HIGH", profile::profile_high()),
                ("NORMAL", profile::profile_normal()),
                ("ROBUST", profile::profile_robust()),
                ("ULTRA", profile::profile_ultra()),
            ] {
                match modem_core::rx::rx(&samples, &config) {
                    Some(result) if result.total_blocks > 0 => {
                        println!(
                            "Profile {}: detected! {}/{} blocks OK, {} bytes decoded",
                            name, result.converged_blocks, result.total_blocks, result.data.len()
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

fn parse_session_id(arg: Option<&str>) -> u32 {
    match arg {
        Some(s) => {
            let cleaned = s.trim_start_matches("0x").trim_start_matches("0X");
            u32::from_str_radix(cleaned, 16).unwrap_or_else(|_| {
                eprintln!("Invalid session_id '{s}' (expected hex, e.g. 'DEADBEEF')");
                std::process::exit(1);
            })
        }
        None => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            nanos ^ (std::process::id() as u32).wrapping_mul(2654435761)
        }
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
                // Keep the extension, truncate the stem to fit
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
