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

        /// Override symbol rate
        #[arg(long)]
        rs: Option<f64>,

        /// Override center frequency
        #[arg(long)]
        fc: Option<f64>,
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

            eprintln!("TX: {} bytes, profile={}, constellation={:?}, LDPC={:?}, Rs={} Bd, beta={}, fc={} Hz",
                data.len(), profile, config.constellation, config.ldpc_rate,
                config.symbol_rate, config.beta, config.center_freq_hz);

            // Generate audio
            let samples = if vox > 0.0 {
                modem_core::tx::tx_with_vox(&data, &config, vox)
            } else {
                modem_core::tx::tx(&data, &config)
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
            rs,
            fc,
        } => {
            let mut config = parse_profile(&profile);
            if let Some(r) = rs {
                config.symbol_rate = r;
            }
            if let Some(f) = fc {
                config.center_freq_hz = f;
            }

            // Read WAV
            let samples = read_wav(&input);
            let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
            eprintln!("RX: {} samples ({:.2}s) from {}", samples.len(), duration_s, input.display());

            // Decode
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

                    // Trim decoded data to payload_length from header if available
                    let payload_len = result.header.as_ref()
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
