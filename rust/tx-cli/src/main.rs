//! TX CLI : envoie un fichier via le modem NBFM single-carrier.
//!
//! Pipeline minimal (sans FEC pour cette premiere version) :
//!   fichier -> bits -> modulateur -> audio 48 kHz stereo -> carte son
//!   PTT (DTR/RTS) actif pendant la transmission
//!   VOX preamble (carrier 1100 Hz) avant les donnees
//!
//! Usage :
//!   nbfm-tx --file image.bin [--mode 16QAM-3/4-1500] [--device "speakers"]
//!           [--serial COM3] [--ptt-line dtr] [--vox-duration 0.5]
//!           [--list-devices]

use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use modem_core::{
    frame::build_frame,
    modulator::{modulate_bytes, vox_tone},
    ModemMode, AUDIO_RATE,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PttLine {
    None,
    Dtr,
    Rts,
}

#[derive(Parser, Debug)]
#[command(name = "nbfm-tx", about = "Transmetteur NBFM single-carrier")]
struct Args {
    /// Fichier a envoyer (binaire, n'importe quel contenu)
    #[arg(short, long)]
    file: Option<String>,

    /// Mode modem : "16QAM-3/4-1500" (defaut), "16QAM-1/2-1600", "8PSK-1/2-1500",
    /// "8PSK-1/2-500", "32QAM-3/4-1200"
    #[arg(short, long, default_value = "16QAM-3/4-1500")]
    mode: String,

    /// Sous-chaine du nom de la carte son (defaut : peripherique de sortie par defaut)
    #[arg(short, long)]
    device: Option<String>,

    /// Port serie pour PTT (ex: COM3 sur Windows, /dev/ttyUSB0 sur Linux)
    #[arg(long)]
    serial: Option<String>,

    /// Ligne PTT : dtr, rts ou none (defaut : none)
    #[arg(long, value_enum, default_value_t = PttLine::None)]
    ptt_line: PttLine,

    /// Duree du carrier VOX au debut (s)
    #[arg(long, default_value_t = 0.5)]
    vox_duration: f32,

    /// Amplitude crete VOX (0..1)
    #[arg(long, default_value_t = 0.6)]
    vox_amplitude: f32,

    /// Frequence du tone VOX (Hz)
    #[arg(long, default_value_t = 1100.0)]
    vox_freq: f32,

    /// Liste les peripheriques audio disponibles et quitte
    #[arg(long, default_value_t = false)]
    list_devices: bool,

    /// Liste les ports serie disponibles et quitte
    #[arg(long, default_value_t = false)]
    list_serial: bool,

    /// Crete audio cible (0..1)
    #[arg(long, default_value_t = 0.5)]
    peak: f32,
}

fn list_audio_devices() -> Result<()> {
    let host = cpal::default_host();
    println!("Hote audio : {:?}", host.id());
    println!("Sortie par defaut : {:?}",
        host.default_output_device().and_then(|d| d.name().ok()));
    println!("\nPeripheriques de sortie :");
    for (i, device) in host.output_devices()?.enumerate() {
        let name = device.name().unwrap_or_else(|_| "?".into());
        let configs: Vec<_> = device.supported_output_configs()
            .map(|c| c.collect::<Vec<_>>()).unwrap_or_default();
        let max_ch = configs.iter().map(|c| c.channels()).max().unwrap_or(0);
        println!("  [{}] {}  (max ch {})", i, name, max_ch);
    }
    Ok(())
}

fn list_serial_ports() -> Result<()> {
    let ports = serialport::available_ports().context("scan ports serie")?;
    println!("Ports serie disponibles :");
    for p in ports {
        println!("  {} ({:?})", p.port_name, p.port_type);
    }
    Ok(())
}

fn pick_output_device(name_substr: Option<&str>)
    -> Result<(cpal::Device, StreamConfig, SampleFormat)>
{
    let host = cpal::default_host();
    let device = if let Some(sub) = name_substr {
        host.output_devices()?
            .find(|d| d.name().map(|n| n.to_lowercase()
                .contains(&sub.to_lowercase())).unwrap_or(false))
            .ok_or_else(|| anyhow!("aucune carte son contenant '{}'", sub))?
    } else {
        host.default_output_device()
            .ok_or_else(|| anyhow!("pas de peripherique audio par defaut"))?
    };
    let name = device.name().unwrap_or_else(|_| "?".into());
    println!("[audio] device : {}", name);

    // Cherche une config 48 kHz stereo f32. Sinon mono. Sinon defaut.
    let supported: Vec<_> = device.supported_output_configs()?.collect();
    let mut chosen: Option<(StreamConfig, SampleFormat)> = None;
    for sc in supported.iter() {
        let min = sc.min_sample_rate().0;
        let max = sc.max_sample_rate().0;
        if AUDIO_RATE >= min && AUDIO_RATE <= max
            && sc.channels() == 2
            && sc.sample_format() == SampleFormat::F32
        {
            let cfg = sc.with_sample_rate(SampleRate(AUDIO_RATE));
            chosen = Some((cfg.config(), sc.sample_format()));
            break;
        }
    }
    if chosen.is_none() {
        // Fallback : prend la config par defaut
        let dcfg = device.default_output_config().context("default output config")?;
        let cfg = StreamConfig {
            channels: dcfg.channels(),
            sample_rate: SampleRate(AUDIO_RATE),
            buffer_size: cpal::BufferSize::Default,
        };
        chosen = Some((cfg, dcfg.sample_format()));
    }
    let (cfg, fmt) = chosen.unwrap();
    println!("[audio] config : {} ch, {} Hz, {:?}",
        cfg.channels, cfg.sample_rate.0, fmt);
    Ok((device, cfg, fmt))
}

fn open_serial_ptt(name: &str, line: PttLine) -> Result<Option<Box<dyn serialport::SerialPort>>> {
    if matches!(line, PttLine::None) {
        return Ok(None);
    }
    let port = serialport::new(name, 9600)
        .timeout(Duration::from_millis(100))
        .open().with_context(|| format!("ouverture {}", name))?;
    println!("[ptt] port serie ouvert : {}", name);
    Ok(Some(port))
}

fn ptt_set(port: &mut Box<dyn serialport::SerialPort>, line: PttLine, on: bool) -> Result<()> {
    match line {
        PttLine::Dtr => port.write_data_terminal_ready(on)?,
        PttLine::Rts => port.write_request_to_send(on)?,
        PttLine::None => {}
    }
    Ok(())
}

/// Envoie le buffer audio (mono f32) sur le device. Met a jour `progress`
/// pour suivre la progression.
fn play_audio(
    audio_mono: Vec<f32>,
    device: &cpal::Device,
    config: &StreamConfig,
) -> Result<()> {
    let progress = Arc::new(AtomicUsize::new(0));
    let progress_cb = Arc::clone(&progress);
    let total = audio_mono.len();
    let channels = config.channels as usize;
    let audio_arc = Arc::new(audio_mono);
    let audio_cb = Arc::clone(&audio_arc);

    let err_fn = |err| eprintln!("[audio] erreur stream : {}", err);

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _info| {
            let pos = progress_cb.load(Ordering::Relaxed);
            let frames = data.len() / channels;
            let mut written = 0usize;
            for f in 0..frames {
                let i = pos + f;
                let v = if i < audio_cb.len() { audio_cb[i] } else { 0.0 };
                for c in 0..channels {
                    data[f * channels + c] = v;
                }
                written += 1;
            }
            progress_cb.fetch_add(written, Ordering::Relaxed);
        },
        err_fn,
        None,
    ).context("build_output_stream")?;
    stream.play().context("stream.play")?;

    // Attend la fin de la lecture
    let target = total + (AUDIO_RATE as usize / 4);  // marge 250 ms
    while progress.load(Ordering::Relaxed) < target {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_devices {
        return list_audio_devices();
    }
    if args.list_serial {
        return list_serial_ports();
    }

    let file = args.file.ok_or_else(|| anyhow!("--file requis"))?;
    let mode = ModemMode::parse(&args.mode)
        .ok_or_else(|| anyhow!("mode inconnu : {}", args.mode))?;
    println!("[modem] mode : {:?}, debit net {} bps", mode, mode.net_bps());

    // Lecture fichier
    let bytes = std::fs::read(&file).with_context(|| format!("lecture {}", file))?;
    let filename = std::path::Path::new(&file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.bin")
        .to_string();
    println!("[file] {} octets ({})", bytes.len(), file);
    println!("[frame] filename='{}'", filename);

    // Assemble frame (header + body) - rs_level=0 pour l'instant
    let frame_bytes = build_frame(mode, 0, 0, true, &filename, &bytes)
        .context("build_frame")?;
    println!("[frame] {} octets total (header 16 + body+padding {})",
        frame_bytes.len(), frame_bytes.len() - 16);

    // Module (LDPC encode + modulation)
    println!("[modem] LDPC encode + modulation...");
    let data_audio = modulate_bytes(&frame_bytes, mode, args.peak);
    let data_dur = data_audio.len() as f32 / AUDIO_RATE as f32;
    println!("[modem] {} samples, {:.2} s",
        data_audio.len(), data_dur);

    // VOX preamble (carrier 1100 Hz)
    let vox = vox_tone(args.vox_duration, args.vox_amplitude, args.vox_freq);
    println!("[vox] {:.2} s @ {} Hz amp {}",
        args.vox_duration, args.vox_freq, args.vox_amplitude);

    // Concatene VOX + 100 ms gap + data + queue 100 ms silence
    let gap = vec![0.0f32; (AUDIO_RATE as f32 * 0.10) as usize];
    let tail = vec![0.0f32; (AUDIO_RATE as f32 * 0.10) as usize];
    let mut audio = Vec::with_capacity(vox.len() + gap.len() + data_audio.len() + tail.len());
    audio.extend_from_slice(&vox);
    audio.extend_from_slice(&gap);
    audio.extend_from_slice(&data_audio);
    audio.extend_from_slice(&tail);

    let total_dur = audio.len() as f32 / AUDIO_RATE as f32;
    println!("[total] {:.2} s a transmettre", total_dur);

    // Audio device
    let (device, config, _fmt) = pick_output_device(args.device.as_deref())?;

    // PTT
    let mut serial = if let Some(s) = &args.serial {
        open_serial_ptt(s, args.ptt_line)?
    } else {
        None
    };
    if let Some(p) = serial.as_mut() {
        println!("[ptt] activation");
        ptt_set(p, args.ptt_line, true)?;
        std::thread::sleep(Duration::from_millis(100));
    }

    // Transmission
    println!("[tx] envoi en cours...");
    let result = play_audio(audio, &device, &config);

    // Desactive PTT meme en cas d'erreur
    if let Some(p) = serial.as_mut() {
        println!("[ptt] desactivation");
        ptt_set(p, args.ptt_line, false)?;
    }

    result?;
    println!("[tx] termine");
    Ok(())
}
