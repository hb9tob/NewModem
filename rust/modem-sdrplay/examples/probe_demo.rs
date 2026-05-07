//! Manual smoke test for the SDRplay backend.
//!
//! Opens the first RSPduo on the bus, configures Tuner B at 145.5 MHz,
//! kicks off RX, and dumps stats for ~5 seconds. Doesn't decode the
//! modem — only validates that the API plumbing + DSP chain produce
//! sane 48 kHz mono audio.
//!
//! Run with:
//! ```text
//! cd rust && cargo run -p modem-sdrplay --example probe_demo
//! ```
//!
//! Requires the SDRplay API service (`sdrplay`) to be running and an
//! RSPduo on USB.

use std::time::{Duration, Instant};

use modem_sdrplay::{rx, AntennaPort, SdrplayConfig, Tuner};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Discover devices first — surfaces a clear error if the daemon
    // is offline or the udev rules are missing.
    let serials = modem_sdrplay::list_serials()?;
    eprintln!(
        "[probe] SDRplay API {:.2} reports {} device(s): {:?}",
        modem_sdrplay::device::api_version(),
        serials.len(),
        serials
    );
    if serials.is_empty() {
        return Err("no SDRplay device on the bus".into());
    }

    // Configure RSPduo Tuner B (50 Ω port, where the bias-T lives)
    // tuned to 145.5 MHz. Keeps every notch + bias-T off so the
    // probe doesn't change the antenna chain unexpectedly.
    let config = SdrplayConfig {
        serial: serials[0].clone(),
        tuner: Tuner::B,
        antenna: AntennaPort::Fifty,
        bias_t: false,
        fm_notch: false,
        dab_notch: false,
        rf_freq_hz: 145_500_000,
        ..SdrplayConfig::default()
    };
    eprintln!(
        "[probe] opening serial={} tuner=B fs={:.3} MS/s decim={} \
         (host {:.0} kSa/s → audio {} Hz, ratio {})",
        config.serial,
        config.sample_rate_hz / 1e6,
        config.decimation,
        config.sample_rate_hz / config.decimation as f64 / 1e3,
        rx::AUDIO_RATE,
        modem_sdrplay::PREFERRED_AUDIO_RATIO
    );
    let (handle, samples) = rx::start(&config)?;
    eprintln!(
        "[probe] streaming: host_iq_rate={} Hz audio_rate={} Hz",
        handle.host_iq_rate_hz, handle.sample_rate
    );

    // Drain the mpsc for ~5 s, then stop.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut total_audio: u64 = 0;
    let mut chunks: u64 = 0;
    let mut last_peak = 0.0f32;
    while Instant::now() < deadline {
        match samples.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                total_audio += chunk.len() as u64;
                chunks += 1;
                let peak = chunk.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
                if peak > last_peak {
                    last_peak = peak;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eprintln!("[probe] no audio for 500 ms — stream stalled?");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("[probe] mpsc disconnected (capture thread exited)");
                break;
            }
        }
    }
    let elapsed = 5.0;
    eprintln!(
        "[probe] {chunks} chunks, {total_audio} audio samples in {elapsed:.1} s \
         → {:.0} Sa/s effective (target {}), peak {:.3}",
        total_audio as f64 / elapsed,
        rx::AUDIO_RATE,
        last_peak
    );
    handle.stop();
    Ok(())
}
