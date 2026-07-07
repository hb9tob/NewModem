//! Pluto TX path: 48 kHz mono audio `Vec<f32>` → iiod-TCP I/Q at 576 / 2304 kSa/s.
//!
//! The contract mirrors `modem_io::traits::SampleSink` (one-shot full
//! buffer submit, returns a progress handle). The TX worker owns its
//! own [`IiodClient`] (one TCP connection per stream — cleanly
//! independent of any concurrent RX worker), runs the radio-faithful
//! upsample + PM chain, packs the result to S16 I/Q, and pushes it
//! into the AD9361 over WRITEBUF.
//!
//! Chain (matches `modem_sdr_dsp`'s integration test layout):
//!
//! ```text
//! Vec<f32> @ 48 kHz audio
//!   → PolyphaseInterpolator (×N to AD9361 IF rate)
//!   → PhaseMod (radio-faithful PM, +6 dB/oct preemph for free)
//!   → pack to S16/16 LE interleaved (peak ±32000 — leaves headroom
//!     under the AD9361 DAC ceiling so PM peaks don't clip)
//!   → iiod WRITEBUF on cf-ad9361-dds-core-lpc
//! ```
//!
//! ## Why chunked streaming and not one big upload
//!
//! Worst-case math: 5 minutes of audio at 48 kHz = 14.4 M f32 samples
//! (57.6 MB). Interpolated to 576 kHz Complex<f32> that's 173 M
//! samples (1.4 GB). Packed S16 I/Q is 691 MB. We can't allocate that
//! up front. The TX worker loops over the input audio in
//! [`TX_CHUNK_AUDIO_SAMPLES`]-sized batches, runs each batch through
//! the chain, packs it to S16 I/Q, and pushes it via `write_buffer`.
//! WRITEBUF blocks until the previous batch has been clocked out
//! kernel-side, which gives natural backpressure against the DAC.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use num_complex::Complex32;

use modem_sdr::telemetry::DemodMode;
use modem_sdr_dsp::ctcss_gen::CtcssToneGen;
use modem_sdr_dsp::interpolator::PolyphaseInterpolator;
use modem_sdr_dsp::pm_mod::PhaseMod;
use modem_sdr_dsp::{ssb_tx_analytic_taps, ssb_tx_interp_taps, ComplexFir, Sideband};

use crate::device::{self, PlutoConfig, PlutoSession};
use crate::error::PlutoError;
use crate::iiod::IiodClient;

/// Audio batch size pulled from the input `Vec<f32>` per chain run.
/// 4096 samples = ~85 ms of audio at 48 kHz. The IIO TX buffer is
/// sized to hold exactly `TX_CHUNK_AUDIO_SAMPLES * ratio` I/Q samples
/// so each WRITEBUF sends only modulated audio — no zero-padding,
/// which would otherwise inject silent gaps between chunks (the chip
/// clocks every sample in the buffer to its DAC, padding included).
const TX_CHUNK_AUDIO_SAMPLES: usize = 4096;

/// AD9361 / AD9363 TX path on Pluto consumes S16/16 LE — full int16
/// range. We scale unit-amplitude `Complex32` PM output by this
/// constant. PhaseMod is constant-envelope (|y| = 1) so peak input
/// is exactly 1.0; we leave a safety margin below `i16::MAX` so any
/// quantization noise doesn't clip.
const TX_S16_SCALE: f32 = 32_000.0;

/// Bytes per scan cycle on Pluto TX: I (i16 LE) + Q (i16 LE).
const BYTES_PER_SCAN: usize = 4;

/// Channel-enable mask for `cf-ad9361-dds-core-lpc`. Bit 0 = voltage0
/// (I), bit 1 = voltage1 (Q). Both on for normal complex TX.
const TX_CHANNEL_MASK: u32 = 0x0000_0003;

/// Kernel-side TX buffer count (double/quad-buffering). The single-buffer
/// default underruns the DAC DMA between WRITEBUF round-trips over a
/// high-latency transport (Pluto over ethernet), silently dropping/repeating
/// data — corrupt symbols with no audible gap, which kills decoding. 4 mirrors
/// libiio (what SDR Console / GNU Radio use, hence why they don't drop).
const TX_KERNEL_BUFFERS: usize = 4;

/// Throttled-error tick.
const STATUS_TICK_PERIOD: Duration = Duration::from_secs(2);

/// `SampleSink`-shaped TX target for Pluto.
///
/// Cheap to clone (just config). The actual iiod connection is opened
/// per-job in [`Self::play_buffer`]; clones don't share any
/// connection state, so the caller can keep one for retune-ish UI
/// concerns while the worker thread runs another.
#[derive(Clone, Debug)]
pub struct PlutoSink {
    config: PlutoConfig,
}

impl PlutoSink {
    pub fn new(config: PlutoConfig) -> Self {
        Self { config }
    }

    /// One-shot full-buffer submit. Spawns a worker thread that
    /// programs the AD9361 via [`device::open`] (transient control
    /// connection), opens its own streaming iiod connection, runs the
    /// upsample + PM chain on `audio_samples` in chunks, and pushes
    /// the resulting I/Q via WRITEBUF on `cf-ad9361-dds-core-lpc`.
    /// Returns a [`TxJob`] the caller polls / drops.
    ///
    /// Returns immediately after the AD9361 has been programmed and
    /// the streaming buffer opened, matching `cpal_capture::start`'s
    /// ready-handshake pattern.
    pub fn play_buffer(&self, audio_samples: Vec<f32>) -> Result<TxJob, PlutoError> {
        let total_samples = audio_samples.len();
        let pos = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), PlutoError>>();

        let pos_thread = pos.clone();
        let stop_thread = stop.clone();
        let cfg = self.config.clone();

        let thread = thread::spawn(move || {
            let _ = run_tx(cfg, audio_samples, pos_thread, stop_thread, ready_tx);
        });

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(TxJob {
                pos,
                total_samples,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(PlutoError::Stream(
                    "timeout starting Pluto TX thread".into(),
                ))
            }
        }
    }

    /// Run TX on an already-configured session — used by the loopback
    /// test where one Pluto serves both directions. Unlike
    /// [`Self::play_buffer`], this is **synchronous**: it returns
    /// after the entire `audio_samples` buffer has been pushed and
    /// drained. Caller spawns its own thread if it wants async.
    pub fn play_on(
        session: PlutoSession,
        audio_samples: &[f32],
        stop: Arc<AtomicBool>,
    ) -> Result<(), PlutoError> {
        run_tx_loop(session, audio_samples, &AtomicUsize::new(0), stop)
    }
}

/// Live handle on a TX job. Same pos / total_samples shape as
/// `modem_io::traits::PlaybackHandle` so the worker's progress code
/// stays unchanged.
pub struct TxJob {
    pub pos: Arc<AtomicUsize>,
    pub total_samples: usize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TxJob {
    pub fn pos(&self) -> usize {
        self.pos.load(Ordering::Relaxed).min(self.total_samples)
    }

    pub fn total_samples(&self) -> usize {
        self.total_samples
    }

    pub fn is_done(&self) -> bool {
        self.pos.load(Ordering::Relaxed) >= self.total_samples
    }

    /// Cancel any pending push and wait for the worker thread to exit.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TxJob {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Emit an unmodulated CW carrier for antenna/PA/QO-100 "tune": a clean tone
/// 1.5 kHz above TX_LO (off the LO-leakage spot). **Full-duplex** — it opens
/// only a TX buffer and writes TX_LO (`altvoltage1`) + TX hardwaregain
/// (`voltage0`); it does NOT touch the sample rate / FIR / RX_LO, so a
/// concurrent RX keeps running and the operator can watch the carrier on the
/// downlink. Blocks until `stop` is set or `max_secs` elapses (a hard safety
/// ceiling). TX attenuation is read live from `atten_centidb` (dB × 100) and
/// written to the AD9361 on every change, so the level can be ridden during tune.
/// Key the PA: power UP the TX LO (`altvoltage1`). The F5OEO Pluto firmware
/// traps this `powerdown` write and pulls the amplifier-chain relays on it
/// (`0` = TX LO on → relays engage). Must be written on a healthy (pre-stream)
/// connection. The RX LO (`altvoltage0`) is untouched, so this stays
/// full-duplex (mandatory for QO-100). Without this write the carrier still
/// radiates but the relays never move — the long-standing reason this app's
/// TX never switched the PA chain.
fn tx_lo_power_up(client: &mut IiodClient) {
    let phy = device::iio_names::PHY;
    let _ = client.write_chn_attr(phy, crate::iiod::ChanDir::Output, "altvoltage1", "powerdown", "0");
}

/// Un-key the PA on a FRESH connection: max the TX attenuation, then power DOWN
/// the TX LO. The firmware traps `altvoltage1 powerdown` = 1 and releases the
/// relays. A post-streaming connection is WRITEBUF-desynced (backpressure
/// short-writes leave it out of step) so its attribute writes silently never
/// reach the firmware — hence a brand-new connection here, mirroring the proven
/// `tx_off` path.
fn tx_lo_power_down(uri: &str) {
    let phy = device::iio_names::PHY;
    if let Ok(mut c) = IiodClient::connect(uri) {
        let _ = c.set_iiod_timeout(2000);
        let _ = c.write_chn_attr(phy, crate::iiod::ChanDir::Output, "voltage0", "hardwaregain", "-89.75");
        let _ = c.write_chn_attr(phy, crate::iiod::ChanDir::Output, "altvoltage1", "powerdown", "1");
        let _ = c.close();
    }
}

/// Live-set the AD9361 TX hardware gain from an attenuation in dB (non-negative;
/// the chip wants it negated) on a FRESH control connection. The gain register
/// is chip-global, so this takes effect immediately even while a TX buffer is
/// streaming on another connection — the same way SDR Console rides TX power
/// mid-transmission. No-op-safe when idle (just sets the register).
pub fn set_tx_hardwaregain(uri: &str, atten_db: f64) -> Result<(), PlutoError> {
    let mut c = IiodClient::connect(uri)
        .map_err(|e| PlutoError::Stream(format!("set TX power connect: {e}")))?;
    let _ = c.set_iiod_timeout(2000);
    let gain = -(atten_db.clamp(0.0, 89.75));
    c.write_chn_attr(
        device::iio_names::PHY,
        crate::iiod::ChanDir::Output,
        "voltage0",
        "hardwaregain",
        &format!("{gain}"),
    )
    .map_err(|e| PlutoError::Stream(format!("set TX power: {e}")))?;
    let _ = c.close();
    Ok(())
}

pub fn run_tune_carrier(
    uri: &str,
    tx_lo_hz: u64,
    sample_rate_hz: u64,
    stop: &AtomicBool,
    atten_centidb: &AtomicU32,
    max_secs: u64,
) -> Result<(), PlutoError> {
    use crate::iiod::ChanDir;
    let phy = device::iio_names::PHY;
    let mut client = IiodClient::connect(uri)
        .map_err(|e| PlutoError::Stream(format!("tune connect: {e}")))?;
    let _ = client.set_iiod_timeout(2000);
    // TX_LO only (altvoltage1) — RX_LO (altvoltage0) is left untouched.
    client
        .write_chn_attr(phy, ChanDir::Output, "altvoltage1", "frequency", &tx_lo_hz.to_string())
        .map_err(|e| PlutoError::Stream(format!("tune TX_LO: {e}")))?;
    // From here on, ANY exit must free the buffer and un-key the PA. Hand the
    // connection to the RAII guard and drive it through `tx.client()`.
    let mut tx = PlutoTxGuard { client: Some(client), buffer_open: false, uri: uri.to_string() };

    // Pre-generate an integer number of 1200 Hz tone periods, so repeatedly
    // pushing the SAME buffer forms a phase-seamless continuous carrier. (The
    // iiod CYCLIC mode returns 0 on this firmware — we do "manual cyclic".)
    let tone_hz = 1200.0_f64;
    let period = (sample_rate_hz as f64 / tone_hz).round().max(1.0) as usize; // 480 @ 576k
    let n = period * 10; // ~4800 IQ — fills the kernel buffer comfortably
    let amp = 0.5_f64 * TX_S16_SCALE as f64; // −6 dBFS digital, clean headroom
    let mut wire = vec![0u8; n * BYTES_PER_SCAN];
    for k in 0..n {
        let ph = std::f64::consts::TAU * tone_hz * (k as f64) / sample_rate_hz as f64;
        let i = (ph.cos() * amp).round() as i16;
        let q = (ph.sin() * amp).round() as i16;
        let off = k * BYTES_PER_SCAN;
        wire[off..off + 2].copy_from_slice(&i.to_le_bytes());
        wire[off + 2..off + 4].copy_from_slice(&q.to_le_bytes());
    }
    // Open the carrier buffer BEFORE keying the PA. A failed OPEN → the guard's
    // Drop runs with relays still OFF and no buffer to free — a clean, wedge-free exit.
    tx.client()
        .open_buffer(device::iio_names::TX_BUFFER, n, TX_CHANNEL_MASK, false)
        .map_err(|e| PlutoError::Stream(format!("tune open TX buffer: {e}")))?;
    tx.buffer_open = true;

    // NOW key the PA (TX LO up → firmware pulls the relays). Full-duplex safe.
    // Released by the guard's Drop on every exit path.
    tx_lo_power_up(tx.client());
    // Baseline: the Tune works, so this is what a HEALTHY ensm_mode looks like on
    // this chip — compare against the SSB TX's ensm to spot where it wedges.
    let ensm = tx
        .client()
        .read_dev_attr(phy, "ensm_mode")
        .unwrap_or_else(|e| format!("<err {e}>"));
    eprintln!(
        "[pluto-tune] carrier ON: TX_LO {} Hz (+{tone_hz} Hz), atten {:.2} dB, ensm_mode={ensm}, ≤{max_secs}s",
        tx_lo_hz,
        atten_centidb.load(Ordering::Relaxed) as f64 / 100.0
    );

    let mut last_centidb = u32::MAX;
    let start = Instant::now();
    while !stop.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(max_secs) {
        // Live power: write TX hardwaregain whenever the attenuation changes.
        let cd = atten_centidb.load(Ordering::Relaxed);
        if cd != last_centidb {
            last_centidb = cd;
            let gain = -(cd as f64 / 100.0); // AD9361 wants negative dB
            let _ = tx
                .client()
                .write_chn_attr(phy, ChanDir::Output, "voltage0", "hardwaregain", &format!("{gain}"));
        }
        // Repeat the same integer-period buffer; on backpressure write_buffer
        // returns 0 and we simply re-push next iteration (no phase drift).
        let _ = tx.client().write_buffer(device::iio_names::TX_BUFFER, &wire);
    }
    // The guard's Drop stops the carrier stream, drops the (WRITEBUF-desynced)
    // connection, and powers down the TX LO on a fresh connection (relays
    // release) — on EVERY exit path now, not just this one.
    eprintln!("[pluto-tune] carrier OFF (TX LO powered down → relays release)");
    Ok(())
}

fn run_tx(
    config: PlutoConfig,
    audio: Vec<f32>,
    pos: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<(), PlutoError>>,
) -> Result<(), PlutoError> {
    // TX open must NOT reprogram the RX LO — a concurrent full-duplex RX
    // (QO-100 downlink monitoring) owns `altvoltage0`. `open` would write
    // the stale persisted `rx_freq_hz` and knock the RX off frequency.
    let session = match device::open_tx(&config) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return Ok(());
        }
    };
    let _ = ready_tx.send(Ok(()));
    run_tx_loop(session, &audio, &pos, stop)
}

fn run_tx_loop(
    session: PlutoSession,
    audio: &[f32],
    pos: &AtomicUsize,
    stop: Arc<AtomicBool>,
) -> Result<(), PlutoError> {
    // Linear mode (QO-100 SSB): the signal goes up as a single sideband, not
    // FM. Dispatch to the SSB-USB modulator; the FM/PM path below is untouched.
    if session.config.rx_demod_mode == DemodMode::SsbUsb {
        return run_tx_loop_ssb(session, audio, pos, stop);
    }
    // Open a dedicated TX streaming connection (independent of any
    // concurrent RX, independent of the control connection that
    // `device::open` opened-and-dropped).
    let mut client = IiodClient::connect(&session.config.uri)?;
    let _ = client.set_iiod_timeout(2000);

    // Key the PA before any RF leaves the chip: power up the TX LO so the F5OEO
    // firmware pulls the amplifier-chain relays (relays-before-drive sequencing).
    // RX LO stays up → full-duplex. Released in the teardown below.
    tx_lo_power_up(&mut client);

    // Buffer size = exactly one chunk's worth of I/Q samples. Sizing
    // it to match means each WRITEBUF ships only modulated audio with
    // no trailing zero-pad — anything else creates audible gaps
    // because the AD9361 DAC clocks every sample in the buffer
    // regardless of whether it carries signal.
    let chunk_iq_samples = TX_CHUNK_AUDIO_SAMPLES * session.negotiated_rate.ratio;
    // Quad-buffer the kernel TX so the DAC DMA stays fed across a high-latency
    // transport (ethernet) — see TX_KERNEL_BUFFERS. Must precede OPEN.
    let _ = client.set_buffers_count(crate::device::iio_names::TX_BUFFER, TX_KERNEL_BUFFERS);
    client
        .open_buffer(
            crate::device::iio_names::TX_BUFFER,
            chunk_iq_samples,
            TX_CHANNEL_MASK,
            false,
        )
        .map_err(|e| PlutoError::Stream(format!("OPEN cf-ad9361-dds-core-lpc: {e}")))?;

    // DSP chain — built once, reused on every chunk.
    let if_rate = session.negotiated_rate.sample_rate_hz as f32;
    let ratio = session.negotiated_rate.ratio;
    // The interpolator's prototype taps: same windowed-sinc design as
    // the matching decimator. 99 taps is plenty for 24 kHz cutoff
    // anti-imaging in front of the AD9361's HB chain.
    let interp_taps = modem_sdr_dsp::decimator::PolyphaseDecimator::hamming_sinc_taps(
        if_rate,
        modem_sdr_dsp::AUDIO_RATE as f32 / 2.0,
        99,
    );
    let mut interp = PolyphaseInterpolator::with_taps(interp_taps, ratio);
    // Scale `k_p` linearly from the 5 kHz calibration so audio at
    // unit amplitude produces ±`session.tx_deviation_hz` on the air.
    let k_p = (session.tx_deviation_hz / 5000.0) * PhaseMod::DEFAULT_K_P;
    let pm = PhaseMod::new(k_p);

    // CTCSS sub-audible tone for repeater squelch. Built only if
    // configured; the audio loop calls `process_add` on each chunk so
    // the tone is summed into the voice *before* the polyphase
    // interpolator and PhaseMod — i.e. it goes through the FM
    // modulator like any other audio component.
    let mut ctcss = if session.ctcss_freq_hz > 0.0 {
        Some(CtcssToneGen::new(
            session.ctcss_freq_hz,
            modem_sdr_dsp::AUDIO_RATE as f32,
            session.ctcss_level,
        ))
    } else {
        None
    };

    let mut last_tick = std::time::Instant::now();
    let mut total_iq_pushed: u64 = 0;
    let mut push_errors: u64 = 0;

    // Pre-allocated wire-format scratch — interleaved I/Q i16 LE.
    // Reused across every WRITEBUF call.
    let buffer_bytes = chunk_iq_samples * BYTES_PER_SCAN;
    let mut wire_buf: Vec<u8> = vec![0u8; buffer_bytes];

    let mut audio_idx = 0usize;
    while audio_idx < audio.len() && !stop.load(Ordering::Relaxed) {
        let end = (audio_idx + TX_CHUNK_AUDIO_SAMPLES).min(audio.len());
        let mut chunk: Vec<f32> = audio[audio_idx..end].to_vec();
        // For a short final chunk, pad the AUDIO side with zeros so
        // the interpolator + PM still produce a full chunk_iq_samples
        // buffer. The trailing zero-audio becomes a constant-phase
        // carrier — silent on the receive side, no audible artifact.
        if chunk.len() < TX_CHUNK_AUDIO_SAMPLES {
            chunk.resize(TX_CHUNK_AUDIO_SAMPLES, 0.0);
        }

        // Mix in the CTCSS sub-audible tone *before* interpolation.
        if let Some(ref mut t) = ctcss {
            t.process_add(&mut chunk);
        }

        // Audio → IF rate via polyphase interpolation.
        let if_audio = interp.process(&chunk);
        debug_assert_eq!(if_audio.len(), chunk_iq_samples);

        // Real audio → Complex<f32> via PhaseMod.
        let mut iq = vec![Complex32::new(0.0, 0.0); if_audio.len()];
        pm.process(&if_audio, &mut iq);

        // Pack to interleaved S16 LE I/Q. AD9361 TX wire format is
        // I_lo, I_hi, Q_lo, Q_hi, repeat — same byte layout the
        // libiio FFI path used to write via channel::write::<i16>.
        for (k, c) in iq.iter().enumerate() {
            let i = (c.re * TX_S16_SCALE).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            let q = (c.im * TX_S16_SCALE).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            let off = k * BYTES_PER_SCAN;
            wire_buf[off..off + 2].copy_from_slice(&i.to_le_bytes());
            wire_buf[off + 2..off + 4].copy_from_slice(&q.to_le_bytes());
        }

        match client.write_buffer(crate::device::iio_names::TX_BUFFER, &wire_buf) {
            Ok(_n) => {
                total_iq_pushed += iq.len() as u64;
            }
            Err(e) => {
                push_errors += 1;
                if last_tick.elapsed() >= STATUS_TICK_PERIOD {
                    eprintln!("[pluto-tx] push error #{push_errors}: {e}");
                    last_tick = std::time::Instant::now();
                }
            }
        }

        audio_idx = end;
        pos.store(audio_idx, Ordering::Relaxed);

        if last_tick.elapsed() >= STATUS_TICK_PERIOD {
            eprintln!(
                "[pluto-tx] tick: {audio_idx}/{} audio samples submitted, \
                 {total_iq_pushed} I/Q pushed, {push_errors} push errors",
                audio.len()
            );
            last_tick = std::time::Instant::now();
        }
    }

    // Best-effort cleanup.
    let _ = client.close_buffer(crate::device::iio_names::TX_BUFFER);
    let _ = client.close();
    // Un-key the PA: drop the relays + silence the TX LO (fresh connection, the
    // streaming one is WRITEBUF-desynced). Relay-safe rest state between bursts.
    tx_lo_power_down(&session.config.uri);
    Ok(())
}

/// Peak target for the SSB I/Q, as a fraction of the i16 DAC full scale. SSB
/// is linear, so the radiated power follows the digital level: we push the
/// whole-burst peak to ~0.92 of full scale — near maximum Pluto output — while
/// staying just under the ceiling (clipping a linear signal splatters the
/// adjacent QO-100 channels). The RF attenuation sets the final power on top.
const SSB_TX_PEAK_TARGET: f32 = 0.92;

/// RAII teardown for a Pluto TX streaming session — shared by the modem SSB
/// burst ([`run_tx_loop_ssb`]) and the CW tune ([`run_tune_carrier`]). On EVERY
/// exit — the normal end, an early `?`-return (e.g. a failed OPEN), or a panic —
/// it frees the kernel DMA buffer (dropping the streaming socket, which the
/// firmware reaps on EOF), drops the connection, and un-keys the PA on a FRESH
/// connection (max attenuation, then TX-LO powerdown → relays release).
///
/// This is what stops a mid-setup failure from stranding the amplifier relays ON
/// and leaking the single `cf-ad9361-dds-core-lpc` buffer — the wedge that left
/// the TX starting ~1-in-10 and killed the CW-tune path until a Pluto reboot.
struct PlutoTxGuard {
    /// Streaming connection; `Some` until Drop takes it to close it.
    client: Option<IiodClient>,
    /// Set once `open_buffer` succeeded, so Drop issues a matching CLOSE.
    buffer_open: bool,
    /// Fresh-connection URI for the relay-safe power-down.
    uri: String,
}

impl PlutoTxGuard {
    fn client(&mut self) -> &mut IiodClient {
        self.client.as_mut().expect("PlutoTxGuard client present until Drop")
    }
}

impl Drop for PlutoTxGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.client.take() {
            // Best-effort CLOSE, then drop the socket. Even if the CLOSE reply is
            // lost on a WRITEBUF-desynced streaming connection, dropping the
            // socket EOFs the firmware, which frees the buffer — so the device
            // never wedges the next OPEN (TX or Tune).
            if self.buffer_open {
                let _ = c.close_buffer(device::iio_names::TX_BUFFER);
            }
            let _ = c.close();
        }
        // Always leave the PA relay-safe on a fresh connection (the streaming one
        // is WRITEBUF-desynced): max attenuation, then TX-LO powerdown → relays off.
        tx_lo_power_down(&self.uri);
    }
}

/// SSB-USB transmit loop — the QO-100 linear uplink. The modem audio is
/// modulated to a single sideband with a suppressed carrier (no FM) and
/// streamed to the AD9361. Mirrors the FM [`run_tx_loop`] lifecycle (dedicated
/// connection, PA keying, relay-safe teardown) but with the linear SSB chain
/// and whole-burst peak normalisation to near full DAC scale.
fn run_tx_loop_ssb(
    session: PlutoSession,
    audio: &[f32],
    pos: &AtomicUsize,
    stop: Arc<AtomicBool>,
) -> Result<(), PlutoError> {
    let mut client = IiodClient::connect(&session.config.uri)?;
    let _ = client.set_iiod_timeout(5000); // a TX-quad cal can take ~1 s

    // Chip state as the SSB TX begins — this is AFTER run_tx's device::open
    // reprogrammed the AD9361 (sample rate, 4× FIR, LO, bandwidth) while the RX
    // was live. 'alert' HERE = device::open itself wedged the chip before any
    // cal/stream; 'fdd'/'tx' = healthy start (so the wedge is later).
    let ensm0 = client
        .read_dev_attr(device::iio_names::PHY, "ensm_mode")
        .unwrap_or_else(|e| format!("<err {e}>"));
    eprintln!("[pluto-tx ssb] START (post device::open): ensm_mode={ensm0}");

    // Per-burst AD9361 TX quadrature calibration — SKIPPED by default. It used to
    // be issued here with `altvoltage1 powerdown = 1` (TX LO commanded OFF): on
    // stock AD9361 semantics (which F5OEO keeps) that powers down the TX RFPLL
    // synth, so `calib_mode = tx_quad` ran against a DEAD LO. tx_quad injects a
    // test tone and measures it through an internal TX→RX loopback; with no LO to
    // upconvert it cannot converge, its error was swallowed, and the AD9361 ENSM
    // was left stuck in ALERT — which radiated NOTHING for the burst AND killed
    // the next CW Tune until a Pluto reboot. It was the ONE chip-state op unique
    // to the SSB path (FM and Tune run no cal and never wedge). Skipping it makes
    // SSB drive the AD9361 exactly like the OTA-proven FM/Tune paths. Cost: no
    // per-burst I/Q image rejection (a faint lower-sideband image remains); a
    // proper ONE-TIME cal with the LO UP belongs in device::open and is bench-
    // gated (LO-up engages the F5OEO PA relays during the ~1 s cal).
    //
    // `PLUTO_SSB_TXQUAD=legacy` restores the exact old (wedging) sequence for a
    // bench A/B — with the cal result and ENSM state now LOGGED, not swallowed.
    let txquad_legacy = std::env::var("PLUTO_SSB_TXQUAD").as_deref() == Ok("legacy");
    if txquad_legacy {
        let _ = client.write_chn_attr(
            device::iio_names::PHY,
            crate::iiod::ChanDir::Output,
            "altvoltage1",
            "powerdown",
            "1",
        );
        let cal = client.write_dev_attr(device::iio_names::PHY, "calib_mode", "tx_quad");
        let ensm = client
            .read_dev_attr(device::iio_names::PHY, "ensm_mode")
            .unwrap_or_else(|e| format!("<err {e}>"));
        eprintln!("[pluto-tx ssb] LEGACY tx_quad (LO down): cal={cal:?} ensm_mode={ensm}");
    } else {
        eprintln!(
            "[pluto-tx ssb] per-burst tx_quad SKIPPED (default; set PLUTO_SSB_TXQUAD=legacy to restore old behaviour)"
        );
    }

    // From here on, ANY exit path must free the buffer and un-key the PA. Hand the
    // connection to the RAII guard and drive it through `tx.client()`.
    let mut tx = PlutoTxGuard {
        client: Some(client),
        buffer_open: false,
        uri: session.config.uri.clone(),
    };

    let ratio = session.negotiated_rate.ratio;
    let if_rate = session.negotiated_rate.sample_rate_hz as u32;
    let ssb_bw = session.config.rx_ssb_bandwidth_hz;
    let chunk_iq_samples = TX_CHUNK_AUDIO_SAMPLES * ratio;

    // Analytic USB at audio rate over the whole burst — the SAME analytic
    // band-pass the SsbRxChain selects with, so TX/RX are exactly symmetric.
    // Computed up front (BEFORE opening the buffer / keying the PA) so a panic in
    // this DSP can never strand an open, keyed buffer. Peak-normalise the whole
    // transmission to near full scale (linear mode → power follows the level).
    let mut analytic = ComplexFir::new(ssb_tx_analytic_taps(ssb_bw, Sideband::Usb));
    let z48 = {
        let audio_c: Vec<Complex32> = audio.iter().map(|&a| Complex32::new(a, 0.0)).collect();
        analytic.process(&audio_c) // audio_c freed here, only z48 persists
    };
    let peak = z48.iter().fold(0.0f32, |m, c| m.max(c.re.hypot(c.im)));
    // Optional digital drive back-off (dB) below the SSB_TX_PEAK_TARGET peak. SSB
    // is linear, so a two-tone (or real modem) signal driven at ~0.92 full scale
    // sits right at the AD9361 TX compression knee → strong in-Pluto IMD3 that RF
    // attenuation (applied AFTER the mixer) cannot remove. Backing the *digital*
    // level down trades output power for linearity. Default 0 dB = byte-identical
    // to the previous behaviour (OTA-preserving); a bench sweep of this vs. the RF
    // attenuation separates Pluto-generated IMD3 from external-amp IMD3.
    let backoff_db: f32 = std::env::var("PLUTO_SSB_DIGITAL_BACKOFF_DB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let backoff_lin = 10f32.powf(-backoff_db.abs() / 20.0);
    if backoff_db.abs() > 1e-3 {
        eprintln!("[pluto-tx ssb] digital drive back-off {backoff_db:.1} dB (×{backoff_lin:.3} below peak target)");
    }
    let scale =
        if peak > 1e-6 { backoff_lin * SSB_TX_PEAK_TARGET * i16::MAX as f32 / peak } else { 0.0 };

    // Quad-buffer the kernel TX so the DAC DMA stays fed across a high-latency
    // transport (ethernet) — the single-buffer default underruns between
    // WRITEBUF round-trips and silently drops/repeats data (corrupt symbols, no
    // audible gap). Mirrors libiio. Must precede OPEN.
    let _ = tx.client().set_buffers_count(crate::device::iio_names::TX_BUFFER, TX_KERNEL_BUFFERS);
    // Open the streaming buffer. If this fails, the guard's Drop runs with the PA
    // still un-keyed (relays OFF) and no buffer to free — a clean, wedge-free exit.
    tx.client()
        .open_buffer(crate::device::iio_names::TX_BUFFER, chunk_iq_samples, TX_CHANNEL_MASK, false)
        .map_err(|e| PlutoError::Stream(format!("OPEN cf-ad9361-dds-core-lpc (ssb): {e}")))?;
    tx.buffer_open = true;

    // ONLY NOW key the PA — after a healthy OPEN — so a failed OPEN can never
    // strand the relays ON. Released by the guard's Drop on every exit path.
    tx_lo_power_up(tx.client());
    // Read back the ENSM state at stream time: 'fdd'/'tx' = healthy; 'alert' means
    // the chip is wedged (e.g. by a legacy tx_quad against a dead LO) and no RF
    // will leave the DAC — pinpoints a wedge without a Pluto in hand.
    let ensm = tx
        .client()
        .read_dev_attr(device::iio_names::PHY, "ensm_mode")
        .unwrap_or_else(|e| format!("<err {e}>"));
    eprintln!("[pluto-tx ssb] PA keyed, ensm_mode={ensm} (expect fdd/tx; 'alert' = wedged), streaming");

    // Anti-imaging interpolation, Re & Im branches (real taps preserve the
    // analytic/USB property and reject the interpolation images).
    let interp_taps = ssb_tx_interp_taps(if_rate);
    let mut interp_re = PolyphaseInterpolator::with_taps(interp_taps.clone(), ratio);
    let mut interp_im = PolyphaseInterpolator::with_taps(interp_taps, ratio);

    let mut wire_buf = vec![0u8; chunk_iq_samples * BYTES_PER_SCAN];
    let mut re48 = vec![0.0f32; TX_CHUNK_AUDIO_SAMPLES];
    let mut im48 = vec![0.0f32; TX_CHUNK_AUDIO_SAMPLES];
    let mut last_tick = std::time::Instant::now();
    let mut total_iq_pushed: u64 = 0;
    let mut push_errors: u64 = 0;

    let mut idx = 0usize;
    while idx < z48.len() && !stop.load(Ordering::Relaxed) {
        let end = (idx + TX_CHUNK_AUDIO_SAMPLES).min(z48.len());
        let m = end - idx;
        for k in 0..TX_CHUNK_AUDIO_SAMPLES {
            // Pad a short final chunk with zeros = silence (SSB has no carrier,
            // so trailing zeros radiate nothing — a clean burst tail).
            let (re, im) = if k < m { (z48[idx + k].re, z48[idx + k].im) } else { (0.0, 0.0) };
            re48[k] = re;
            im48[k] = im;
        }
        let re_if = interp_re.process(&re48);
        let im_if = interp_im.process(&im48);
        for k in 0..re_if.len() {
            let i = (re_if[k] * scale).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            let q = (im_if[k] * scale).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            let off = k * BYTES_PER_SCAN;
            wire_buf[off..off + 2].copy_from_slice(&i.to_le_bytes());
            wire_buf[off + 2..off + 4].copy_from_slice(&q.to_le_bytes());
        }
        match tx.client().write_buffer(crate::device::iio_names::TX_BUFFER, &wire_buf) {
            Ok(_n) => total_iq_pushed += re_if.len() as u64,
            Err(e) => {
                push_errors += 1;
                if last_tick.elapsed() >= STATUS_TICK_PERIOD {
                    eprintln!("[pluto-tx ssb] push error #{push_errors}: {e}");
                    last_tick = std::time::Instant::now();
                }
            }
        }
        idx = end;
        pos.store(idx, Ordering::Relaxed);
        if last_tick.elapsed() >= STATUS_TICK_PERIOD {
            eprintln!(
                "[pluto-tx ssb] tick: {idx}/{} audio samples, {total_iq_pushed} I/Q pushed, \
                 {push_errors} push errors (peak-norm ×{scale:.0})",
                z48.len()
            );
            last_tick = std::time::Instant::now();
        }
    }

    if total_iq_pushed == 0 {
        eprintln!(
            "[pluto-tx ssb] WARNING: 0 I/Q samples reached the DAC ({push_errors} push errors) \
             — nothing was transmitted (check ensm_mode above)"
        );
    }

    // Normal end: the guard's Drop frees the buffer, drops the connection, and
    // un-keys the PA (relay-safe). The same teardown now runs on EVERY exit path
    // (early `?`-return, panic), not just this happy one.
    Ok(())
}
