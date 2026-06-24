//! Pluto RX path: iiod-TCP I/Q stream → 48 kHz mono `Vec<f32>` audio.
//!
//! Mirrors the shape of [`modem_io::cpal_capture`] so `modem-worker`
//! can plug a Pluto into the same `Receiver<Vec<f32>>` consumer it
//! already uses for the soundcard. The capture thread owns its own
//! [`IiodClient`] (one TCP connection per stream — iiod allocates one
//! server thread per client, the AD9361 hardware itself serializes
//! contention), runs the radio-faithful DSP chain straight after the
//! buffer pump, and forwards 48 kHz mono `Vec<f32>` batches over an
//! mpsc.
//!
//! Chain — delegates the DSP entirely to
//! [`modem_sdr_dsp::NbfmRxChain`]; this module owns transport (iiod
//! buffer pump, S12-in-S16 sample format, AD9363 rate negotiation)
//! and the shared crate owns the math. SDRplay uses the same chain
//! with a non-zero `lo_offset_hz` to compensate the LO-offset tuning
//! its tuner needs; Pluto's AD9363 has hardware DC compensation so
//! the LO sits straight on the user's frequency and `lo_offset_hz`
//! is 0.
//!
//! ```text
//! cf-ad9361-lpc → I[i16], Q[i16]                            (576 kHz IF)
//!   → Complex32 = (I + jQ) / 2048                           (S12-aligned)
//!   → NbfmRxChain.process()
//!         ↪ FreqXlatingFir (LPF + decim → 48 kHz I/Q at DC)
//!         ↪ QuadratureDemod (at 48 kHz)
//!         ↪ DeemphasisLpf, SubAudioHpf
//!   → mpsc::Sender<Vec<f32>> (48 kHz mono audio)
//! ```
//!
//! The thread starts by re-running [`device::open`] inside itself,
//! which programs the AD9361 over a transient control connection and
//! returns a config snapshot. The streaming connection is then opened
//! separately and lives for the duration of the capture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use num_complex::Complex32;

use modem_sdr::config::{GainSetting, ManualGainValue};
use modem_sdr::telemetry::{RadioCommand, RadioTelemetry};
use modem_sdr_dsp::{NbfmRxChain, NbfmRxChainConfig};
use modem_sdr_radio::{RadioHardware, RadioInit, RadioRuntime};

use crate::device::{self, NegotiatedRate, PlutoConfig, PlutoSession, RxGainMode};
use crate::error::PlutoError;
use crate::iiod::{ChanDir, IiodClient};

/// Audio rate the chain produces. Locked to the modem core's
/// `AUDIO_RATE`. Decimation ratio against the AD9361's IF rate is
/// [`crate::PREFERRED_RATIO`] (or [`crate::FALLBACK_RATIO`] on
/// fallback).
pub const AUDIO_RATE: u32 = modem_sdr_dsp::AUDIO_RATE;

/// IIO buffer size, in scan cycles (= I/Q sample pairs). At 576 kHz,
/// 8192 is ~14 ms per refill — small enough that the GUI feels
/// responsive but big enough that USB scheduling jitter doesn't
/// starve the buffer. Multiple of 16 to match the AD9361 driver's
/// `length_align_bytes` on `cf-ad9361-lpc`.
const RX_BUFFER_SAMPLES: usize = 8192;

/// Bytes per scan cycle on Pluto RX: I (i16 LE) + Q (i16 LE).
const BYTES_PER_SCAN: usize = 4;

/// Channel-enable mask for `cf-ad9361-lpc`. Bit 0 = voltage0 (I),
/// bit 1 = voltage1 (Q). Both on for normal complex RX.
const RX_CHANNEL_MASK: u32 = 0x0000_0003;

/// Audio samples per `Vec<f32>` pushed into the worker mpsc. 1200
/// samples = 25 ms at 48 kHz, which matches what the cpal soundcard
/// backend delivers per callback. One refill of [`RX_BUFFER_SAMPLES`]
/// produces ~683 audio samples after ÷12 decimation, so we accumulate
/// 2-3 refills' worth before flushing. Equalising the chunk shape
/// between cpal and Pluto keeps the worker side homogeneous.
const TARGET_CHUNK_SAMPLES: usize = 1200;

/// AD9361 / AD9363 RX path on Pluto outputs S12 in S16 LE — a 12-bit
/// signed sample sign-extended into a 16-bit word, peak ±2047. We
/// scale to unit-amplitude `Complex32` by dividing by this constant.
const RX_S12_PEAK: f32 = 2047.0;

/// How often the capture thread emits a status / error tick to stderr.
/// Mirrors the throttling pattern from `modem_io::cpal_capture` so a
/// USB hiccup doesn't flood the terminal.
const STATUS_TICK_PERIOD: Duration = Duration::from_secs(2);

// Radio-tab FFT sizes and emit-rate throttles now live in the shared
// `modem_sdr_radio::RadioRuntime`.

/// The two channel ends the worker hands the capture thread to drive
/// the Radio tab: telemetry out (spectra / S-meter / tune state) and
/// control in (retune / gain / squelch). `None` for plain captures.
struct RadioWiring {
    telemetry_tx: Sender<RadioTelemetry>,
    control_rx: Receiver<RadioCommand>,
}

/// Live handle on a Pluto capture thread. Drop / call [`stop()`] to
/// cancel streaming.
pub struct CaptureHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    pub sample_rate: u32,
    pub channels: u16,
    /// AD9363 sample rate that the BBPLL locked at — typically 576 kHz
    /// or 2304 kHz on fallback. Reported for the GUI / CLI.
    pub negotiated_iq_rate_hz: u64,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Open a Pluto for receive, kick off the capture thread, and return a
/// channel of 48 kHz mono f32 batches.
///
/// The signature mirrors `modem_io::cpal_capture::start(&str)` — same
/// `(handle, mpsc::Receiver<Vec<f32>>)` tuple — so the worker doesn't
/// know it's talking to an SDR rather than a soundcard.
pub fn start(config: &PlutoConfig) -> Result<(CaptureHandle, Receiver<Vec<f32>>), PlutoError> {
    let (handle, sample_rx) = start_inner(config, None)?;
    Ok((handle, sample_rx))
}

/// Like [`start`] but also wires the Radio-tab telemetry / control
/// channels into the capture thread. Returns, in addition to the audio
/// receiver, the telemetry receiver (RF/audio spectrum, S-meter, tune
/// state) and the control sender (live retune / gain / squelch).
pub fn start_radio(
    config: &PlutoConfig,
) -> Result<
    (
        CaptureHandle,
        Receiver<Vec<f32>>,
        Receiver<RadioTelemetry>,
        Sender<RadioCommand>,
    ),
    PlutoError,
> {
    let (telemetry_tx, telemetry_rx) = mpsc::channel::<RadioTelemetry>();
    let (control_tx, control_rx) = mpsc::channel::<RadioCommand>();
    let wiring = RadioWiring {
        telemetry_tx,
        control_rx,
    };
    let (handle, sample_rx) = start_inner(config, Some(wiring))?;
    Ok((handle, sample_rx, telemetry_rx, control_tx))
}

/// Shared spawn path for [`start`] / [`start_radio`].
fn start_inner(
    config: &PlutoConfig,
    radio: Option<RadioWiring>,
) -> Result<(CaptureHandle, Receiver<Vec<f32>>), PlutoError> {
    let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<NegotiatedRate, PlutoError>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let cfg = config.clone();

    let thread = thread::spawn(move || {
        run_capture(cfg, sample_tx, ready_tx, stop_thread, radio);
    });

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(rate)) => Ok((
            CaptureHandle {
                stop,
                thread: Some(thread),
                sample_rate: AUDIO_RATE,
                channels: 1,
                negotiated_iq_rate_hz: rate.sample_rate_hz,
            },
            sample_rx,
        )),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(PlutoError::Stream(
            "timeout waiting for Pluto capture thread to start".into(),
        )),
    }
}

/// Same as [`start`] but takes an already-configured [`PlutoSession`].
/// Used by the loopback test where one configured device serves both
/// directions; production callers should prefer [`start`].
pub fn start_on(
    session: PlutoSession,
    sample_tx: Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<NegotiatedRate, PlutoError> {
    capture_loop(session, sample_tx, stop, None)
}

fn run_capture(
    config: PlutoConfig,
    sample_tx: Sender<Vec<f32>>,
    ready_tx: Sender<Result<NegotiatedRate, PlutoError>>,
    stop: Arc<AtomicBool>,
    radio: Option<RadioWiring>,
) {
    let session = match device::open(&config) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let rate = session.negotiated_rate;
    let _ = ready_tx.send(Ok(rate));
    let _ = capture_loop(session, sample_tx, stop, radio);
}

/// Inner loop. Returns once `stop` is set or the iiod stream fails.
/// Used both by [`run_capture`] and by [`start_on`].
fn capture_loop(
    session: PlutoSession,
    sample_tx: Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    radio: Option<RadioWiring>,
) -> Result<NegotiatedRate, PlutoError> {
    // Open the streaming connection — distinct from the control
    // connection that `device::open` used and dropped. iiod allocates
    // one server thread per client, so this is contention-free.
    let mut client = IiodClient::connect(&session.config.uri)?;
    // Generous server-side timeout so a slow first refill (chip
    // calibration) doesn't trip a benign error.
    let _ = client.set_iiod_timeout(2000);

    client
        .open_buffer(
            crate::device::iio_names::RX_BUFFER,
            RX_BUFFER_SAMPLES,
            RX_CHANNEL_MASK,
            false,
        )
        .map_err(|e| PlutoError::Stream(format!("OPEN cf-ad9361-lpc: {e}")))?;

    // DSP chain — built once, reused on every refill. The chain
    // takes the host I/Q rate (576 kS/s preferred, 2304 kS/s on
    // BBPLL-fallback) plus the operator-selected max deviation, runs
    // the channel-select FreqXlatingFir + discriminator + audio
    // filters, and outputs 48 kHz mono. lo_offset_hz = 0 because
    // the AD9363 has hardware DC compensation — no LO-leakage spike
    // to dodge.
    let mut chain = NbfmRxChain::new(NbfmRxChainConfig::new(
        session.negotiated_rate.sample_rate_hz as u32,
        session.rx_max_deviation_hz,
        0.0,
    ));

    // Radio-tab runtime: spectra / S-meter / tune-state telemetry and live
    // retune. `None` for plain captures (CLI, loopback) — the loop costs
    // nothing extra then. The shared runtime owns the DSP/telemetry; the
    // control receiver is drained inline below (this is a poll loop, not a
    // callback, so it may safely touch the hardware) and the Pluto-specific
    // LO/gain writes go through `PlutoHardware`. lo_offset = 0 (AD9363 DC
    // comp), so digital_offset starts at 0 and displayed_rf = the LO.
    let (mut radio_rt, mut radio_ctl, mut radio_hw) = match radio {
        Some(w) => {
            let init = RadioInit {
                input_rate_hz: session.negotiated_rate.sample_rate_hz as u32,
                lo_hz: session.config.rx_freq_hz,
                digital_offset_hz: 0.0,
                lo_offset_hz: 0.0,
                max_deviation_hz: session.rx_max_deviation_hz,
                dc_tunable: true,
            };
            (
                Some(RadioRuntime::new(w.telemetry_tx, init)),
                Some(w.control_rx),
                Some(PlutoHardware {
                    uri: session.config.uri.clone(),
                }),
            )
        }
        None => (None, None, None),
    };

    // Pre-allocated I/Q wire-format scratch — one buffer worth of
    // bytes. iiod's `read_buffer_into` writes here; we reinterpret
    // i16 LE pairs after the fill.
    let buffer_bytes = RX_BUFFER_SAMPLES * BYTES_PER_SCAN;
    let mut wire_buf = vec![0u8; buffer_bytes];

    let mut last_tick = std::time::Instant::now();
    let mut refill_errors: u64 = 0;
    let mut total_iq_samples: u64 = 0;

    // Aggregation buffer between refills — one refill produces
    // ~RX_BUFFER_SAMPLES/ratio audio samples (~683 at 576 kHz ÷12),
    // smaller than [`TARGET_CHUNK_SAMPLES`]. We accumulate until we
    // have a full chunk, then flush.
    let mut pending: Vec<f32> = Vec::with_capacity(TARGET_CHUNK_SAMPLES * 2);

    while !stop.load(Ordering::Relaxed) {
        let n = match client.read_buffer_into(crate::device::iio_names::RX_BUFFER, &mut wire_buf) {
            Ok(n) => n,
            Err(e) => {
                refill_errors += 1;
                if last_tick.elapsed() >= STATUS_TICK_PERIOD {
                    eprintln!("[pluto-rx] refill error #{refill_errors}: {e}");
                    last_tick = std::time::Instant::now();
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
        };
        if n == 0 {
            continue;
        }

        // Reinterpret the wire bytes as (I, Q) i16 LE pairs. Pluto's
        // RX is S12 in S16 LE: sample bits in the low 12 with sign
        // extension to 16, peak ±2047. We scale to unit-amplitude
        // Complex32 by dividing by RX_S12_PEAK.
        let n_pairs = n / BYTES_PER_SCAN;
        total_iq_samples += n_pairs as u64;

        let mut iq: Vec<Complex32> = Vec::with_capacity(n_pairs);
        for pair in wire_buf[..n].chunks_exact(BYTES_PER_SCAN) {
            let i = i16::from_le_bytes([pair[0], pair[1]]) as f32 / RX_S12_PEAK;
            let q = i16::from_le_bytes([pair[2], pair[3]]) as f32 / RX_S12_PEAK;
            iq.push(Complex32::new(i, q));
        }

        // Radio control + wideband spectrum: drain any pending retune /
        // gain / squelch commands (may rebuild `chain`), then run the
        // RF FFT on the raw I/Q (before downmix). Both throttled inside.
        if let (Some(rt), Some(ctl), Some(hw)) =
            (radio_rt.as_mut(), radio_ctl.as_mut(), radio_hw.as_mut())
        {
            while let Ok(cmd) = ctl.try_recv() {
                RadioRuntime::run_hardware(&cmd, hw);
                rt.apply_command(cmd, &mut chain);
            }
            rt.on_iq(&iq);
        }

        // Channel select + demod + audio filters in one call. The
        // chain handles every rate transition; we get 48 kHz mono
        // back.
        let mut audio = chain.process(&iq);
        if audio.is_empty() {
            continue;
        }

        // Radio audio telemetry (S-meter + audio spectrum) and squelch.
        // A muted chunk is zeroed so neither the monitor nor the decode
        // path hears noise below the threshold.
        if let Some(rt) = radio_rt.as_mut() {
            if rt.on_audio(&audio, chain.last_channel_power()) {
                audio.iter_mut().for_each(|s| *s = 0.0);
            }
        }

        // Accumulate into `pending`, flush full chunks of
        // [`TARGET_CHUNK_SAMPLES`].
        pending.extend_from_slice(&audio);
        let mut hung_up = false;
        while pending.len() >= TARGET_CHUNK_SAMPLES {
            let chunk: Vec<f32> = pending.drain(..TARGET_CHUNK_SAMPLES).collect();
            if sample_tx.send(chunk).is_err() {
                hung_up = true;
                break;
            }
        }
        if hung_up {
            break;
        }

        if last_tick.elapsed() >= STATUS_TICK_PERIOD {
            eprintln!(
                "[pluto-rx] tick: {total_iq_samples} I/Q samples in, {refill_errors} refill errors"
            );
            last_tick = std::time::Instant::now();
        }
    }

    // Best-effort cleanup. If CLOSE fails (server already gone, e.g.
    // user yanked the USB) we don't care — the connection drops on
    // the next `client` deref-Drop.
    let _ = client.close_buffer(crate::device::iio_names::RX_BUFFER);
    let _ = client.close();
    Ok(session.negotiated_rate)
}

/// Live-tuning hardware shim for the Radio tab: turns the backend-agnostic
/// [`RadioHardware`] calls into AD9361 iiod attribute writes over a
/// transient control connection. The shared `RadioRuntime` owns the DSP /
/// telemetry; only the LO / gain writes are Pluto-specific.
struct PlutoHardware {
    uri: String,
}

impl RadioHardware for PlutoHardware {
    fn retune_lo(&mut self, lo_hz: u64) {
        if let Err(e) = live_lo_write(&self.uri, lo_hz) {
            eprintln!("[pluto-rx] live LO write to {lo_hz} Hz failed: {e}");
        }
    }

    fn set_gain(&mut self, gain: &GainSetting) {
        if let Err(e) = apply_gain_live(&self.uri, gain) {
            eprintln!("[pluto-rx] live gain change failed: {e}");
        }
    }
}

/// Live hardware-LO retune over a transient iiod control connection.
/// iiod allocates one server thread per client and the AD9361
/// serialises hardware contention, so opening a short-lived control
/// connection alongside the streaming one is clean — and far simpler
/// than multiplexing control writes onto the busy streaming socket.
/// Retunes are operator-driven (rare), so the connect cost is moot.
pub(crate) fn live_lo_write(uri: &str, lo_hz: u64) -> Result<(), PlutoError> {
    let mut c = IiodClient::connect(uri)?;
    let _ = c.set_iiod_timeout(2000);
    c.write_chn_attr(
        device::iio_names::PHY,
        ChanDir::Output,
        "altvoltage0",
        "frequency",
        &lo_hz.to_string(),
    )
    .map_err(|e| PlutoError::Stream(format!("live RX_LO write {lo_hz} Hz: {e}")))?;
    c.close()?;
    Ok(())
}

/// Live RX-gain change over a transient control connection. Maps the
/// backend-agnostic [`GainSetting`] onto the AD9361's
/// `gain_control_mode` + `hardwaregain` the same way `device::open`
/// does at session start.
fn apply_gain_live(uri: &str, gain: &GainSetting) -> Result<(), PlutoError> {
    let (mode, db) = match gain {
        GainSetting::Manual(ManualGainValue::Db { db }) => (RxGainMode::Manual, *db),
        GainSetting::Manual(_) => (RxGainMode::Manual, 30),
        GainSetting::AgcMode { id, .. } => {
            (RxGainMode::from_iio_str(id).unwrap_or(RxGainMode::SlowAttack), 30)
        }
    };
    let mut c = IiodClient::connect(uri)?;
    let _ = c.set_iiod_timeout(2000);
    c.write_chn_attr(
        device::iio_names::PHY,
        ChanDir::Input,
        "voltage0",
        "gain_control_mode",
        mode.as_iio_str(),
    )
    .map_err(|e| PlutoError::Stream(format!("live gain_control_mode: {e}")))?;
    if mode == RxGainMode::Manual {
        c.write_chn_attr(
            device::iio_names::PHY,
            ChanDir::Input,
            "voltage0",
            "hardwaregain",
            &db.to_string(),
        )
        .map_err(|e| PlutoError::Stream(format!("live hardwaregain {db} dB: {e}")))?;
    }
    c.close()?;
    Ok(())
}
