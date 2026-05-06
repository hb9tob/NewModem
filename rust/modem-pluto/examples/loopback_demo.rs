//! Live end-to-end loopback test: TX chain → AD9361 FPGA digital
//! loopback → RX chain.
//!
//! What this proves: the full modem-pluto code path (chunked stream
//! TX, S16 pack, AD9361 buffer push, AD9361 buffer refill, S12
//! unpack, QuadratureDemod, polyphase decim, emphasis filters) round-
//! trips a known audio signal back to itself within deemph-passband
//! tolerance. This is the ground-truth verification for task #11.
//!
//! ## How the loopback path works
//!
//! The AD9361 has a debug attribute `loopback` on `ad9361-phy` that,
//! when set to `1`, routes the TX FPGA datapath straight to the RX
//! FPGA datapath inside the chip — no analog front-end, no antenna,
//! no leakage. That gives a deterministic test that doesn't depend
//! on RF coupling.
//!
//! `industrial-io 0.6` doesn't expose debug-attribute writes, so we
//! shell out to `iio_attr -D ad9361-phy loopback <0|1>`. This is the
//! one place modem-pluto cheats around the Rust API.
//!
//! ## Run
//!
//! ```text
//! cargo run -p modem-pluto --example loopback_demo
//! ```
//!
//! Optional positional arg: libiio URI (default `usb:1.6.5`).

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use modem_pluto::device::{self, PlutoConfig};
use modem_pluto::{rx, tx};

const TONE_HZ: f32 = 1_000.0;
const TONE_AMP: f32 = 0.5; // peak audio amplitude, well below clipping
const TONE_DURATION_S: f32 = 2.0;
const RX_DRAIN_S: f32 = 3.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args()
        .nth(1)
        .unwrap_or_else(|| modem_pluto::DEFAULT_URI.to_string());
    let config = PlutoConfig {
        uri: uri.clone(),
        // The freqs don't matter for FPGA loopback (the analog
        // front-end is bypassed) but the AD9361 still wants valid
        // tuner settings. 145.5 MHz is a fine ham-band default.
        rx_freq_hz: 145_500_000,
        tx_freq_hz: 145_500_000,
        rx_gain_mode: modem_pluto::device::RxGainMode::Manual,
        rx_gain_db: 30,
        tx_attenuation_db: 30.0,
        rf_bandwidth_hz: 200_000,
        prefer_low_rate: true,
        rx_max_deviation_hz: 5000.0,
        tx_deviation_hz: 5000.0,
        ctcss_freq_hz: 0.0,
        ctcss_level: 0.1,
    };

    // libiio's USB backend gives ONE client at a time — so we must
    // toggle the debug `loopback` attribute via `iio_attr` BEFORE
    // opening our own Context (the open will then claim the interface
    // for the rest of the test). Same dance on the way out.
    println!("[loopback] enabling FPGA digital loopback");
    set_fpga_loopback(&uri, true)?;
    let _guard = LoopbackGuard {
        uri: uri.clone(),
        active: true,
    };

    println!("[loopback] opening Pluto at {uri}");
    let session = device::open(&config)?;
    println!(
        "[loopback] negotiated rate: {} Hz (ratio ÷{})",
        session.negotiated_rate.sample_rate_hz, session.negotiated_rate.ratio
    );

    // Generate a 1 kHz sine, peak amplitude TONE_AMP, TONE_DURATION_S
    // long, at the modem core's 48 kHz audio rate.
    let total_samples = (rx::AUDIO_RATE as f32 * TONE_DURATION_S) as usize;
    let tone: Vec<f32> = (0..total_samples)
        .map(|n| {
            TONE_AMP
                * (2.0 * std::f32::consts::PI * TONE_HZ * n as f32 / rx::AUDIO_RATE as f32).sin()
        })
        .collect();

    // Spawn RX before TX so we don't miss the head of the tone.
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = std::sync::mpsc::channel::<Vec<f32>>();
    let rx_session = session.clone();
    let rx_stop = stop.clone();
    let rx_thread = thread::spawn(move || {
        let _ = rx::start_on(rx_session, sample_tx, rx_stop);
    });
    // Give the RX thread a moment to allocate its libiio buffer.
    thread::sleep(Duration::from_millis(200));

    println!("[loopback] pushing {TONE_DURATION_S} s of {TONE_HZ} Hz tone via TX");
    let tx_stop = stop.clone();
    let tx_session = session.clone();
    let tx_thread = thread::spawn(move || {
        let _ = tx::PlutoSink::play_on(tx_session, &tone, tx_stop);
    });

    // Drain RX for RX_DRAIN_S seconds.
    let drain_start = Instant::now();
    let mut received: Vec<f32> = Vec::new();
    while drain_start.elapsed().as_secs_f32() < RX_DRAIN_S {
        if let Ok(chunk) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            received.extend(chunk);
        }
    }
    println!(
        "[loopback] drained {} audio samples ({:.2} s)",
        received.len(),
        received.len() as f32 / rx::AUDIO_RATE as f32
    );

    // Dump the captured audio to /tmp/pluto_loopback.wav for offline
    // inspection — a quick `audacity /tmp/pluto_loopback.wav` shows
    // the waveform and an FFT in seconds.
    let wav_path = std::env::temp_dir().join("pluto_loopback.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rx::AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    if let Ok(mut w) = hound::WavWriter::create(&wav_path, spec) {
        for &s in &received {
            let v = (s.clamp(-1.0, 1.0) * 32_000.0) as i16;
            let _ = w.write_sample(v);
        }
        let _ = w.finalize();
        println!("[loopback] dumped capture to {}", wav_path.display());
    }

    // Stop both directions and tear the session down so the USB
    // interface is free for the LoopbackGuard's iio_attr call.
    stop.store(true, Ordering::Relaxed);
    let _ = tx_thread.join();
    let _ = rx_thread.join();
    drop(session);
    // The Arc<InnerContext> in industrial-io may take a moment to drop
    // after the threads exit (Buffer destructors run on each thread's
    // local Channel handles). Yield once to let that finalize before
    // _guard runs iio_attr.
    thread::sleep(Duration::from_millis(200));

    // ----- Verify the received audio --------------------------------
    // Skip the first 0.3 s (RX/TX startup transient + DSP filter
    // ring-up + chip path delay) and analyze a clean 0.5 s window.
    let skip = (rx::AUDIO_RATE as f32 * 0.3) as usize;
    let window = (rx::AUDIO_RATE as f32 * 0.5) as usize;
    if received.len() < skip + window {
        return Err(format!(
            "not enough audio captured: {} samples, need at least {}",
            received.len(),
            skip + window
        )
        .into());
    }
    let analysis = &received[skip..skip + window];

    // RMS — a real signal should sit well above the noise floor.
    let rms = (analysis.iter().map(|&x| x * x).sum::<f32>() / analysis.len() as f32).sqrt();
    println!("[loopback] analysis RMS: {rms:.4}");

    // Zero-crossing rate — for a clean sine of frequency f, ZC rate
    // is 2f. Tolerant ±5 % to absorb the deemph corner shifting the
    // dominant period a tiny bit.
    let zc = zero_crossings(analysis);
    let est_freq = zc as f32 * rx::AUDIO_RATE as f32 / (2.0 * analysis.len() as f32);
    println!(
        "[loopback] zero crossings = {zc}, estimated freq = {est_freq:.1} Hz \
         (target {TONE_HZ})"
    );

    let pass = rms > 0.05 && (est_freq - TONE_HZ).abs() / TONE_HZ < 0.05;
    if pass {
        println!("[loopback] ✅ PASS — round-trip recovered the {TONE_HZ} Hz tone");
        Ok(())
    } else {
        Err(format!(
            "loopback verification failed: rms = {rms:.4} (need > 0.05), \
             freq = {est_freq:.1} Hz (need within 5 % of {TONE_HZ})"
        )
        .into())
    }
}

/// Count sign changes (positive → negative or vice versa). For a
/// signal of frequency f sampled at fs over N samples, the count is
/// `2 * f * N / fs`.
fn zero_crossings(samples: &[f32]) -> usize {
    samples
        .windows(2)
        .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
        .count()
}

fn set_fpga_loopback(uri: &str, enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    let val = if enable { "1" } else { "0" };
    let status = Command::new("iio_attr")
        .args(["-u", uri, "-D", "ad9361-phy", "loopback", val])
        .status()?;
    if !status.success() {
        return Err(format!("iio_attr loopback {val} returned {status}").into());
    }
    Ok(())
}

/// RAII restorer — flips loopback back off on drop. Works even if the
/// test panics partway through, so we don't leave the chip stuck in
/// loopback mode after a failure.
struct LoopbackGuard {
    uri: String,
    active: bool,
}

impl Drop for LoopbackGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = set_fpga_loopback(&self.uri, false);
        }
    }
}
