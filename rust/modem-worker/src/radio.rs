//! Radio-tab session: hybrid tuner policy + capture↔GUI bridge.
//!
//! Two pieces:
//!   - [`RadioTuner`] — a pure, hardware-free state machine that decides,
//!     for a requested RF frequency, whether to fine-tune digitally (a
//!     glitch-free NCO offset inside the captured band) or retune the
//!     hardware LO. It also knows how to recenter the LO. Fully unit-
//!     tested; no IO, no threads.
//!   - [`spawn_session`] — a thread that sits between the SDR capture
//!     thread and the RX decode worker. It forwards the decoded audio on
//!     unchanged (so the decode worker is untouched), tees a copy to an
//!     optional [`AudioMonitor`] for live listening, forwards capture
//!     telemetry (spectra / S-meter / tune state) to the GUI as named
//!     events, and translates GUI [`RadioUiCommand`]s into backend
//!     [`RadioCommand`]s via the tuner.
//!
//! Why a separate thread rather than folding this into `rx_worker`: the
//! decode loop is OTA-critical and stays byte-for-byte unchanged. The
//! monitor's cpal `Stream` is `!Send` on Windows, so it must be owned by
//! one thread — this one — which also has the audio to feed it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use modem_io::AudioMonitor;
use modem_sdr::config::GainSetting;
use modem_sdr::telemetry::{RadioCommand, RadioTelemetry};
use modem_sdr::RadioTuning;

use crate::event_sink::{EventSink, EventSinkExt};

/// Audio sample rate of the monitored stream (matches the modem core).
const MONITOR_SAMPLE_RATE: u32 = 48_000;

/// Poll period on the audio input — also the worst-case latency before a
/// queued GUI command or telemetry frame is serviced.
const TICK: Duration = Duration::from_millis(20);

/// Hybrid digital-NCO + hardware-LO tuner. Pure policy: given a target RF
/// frequency it returns the [`RadioCommand`] to realise it, updating its
/// own model of where the receiver sits. No hardware, no IO.
///
/// Frequency model: the operator listens at `displayed_rf = lo +
/// digital_offset`. Small moves change `digital_offset` only (the
/// captured band already contains the wanted signal); large moves — or
/// moves that would put the channel on DC for a zero-IF tuner — retune
/// the LO and reset the offset to its nominal value.
#[derive(Clone, Debug)]
pub struct RadioTuner {
    displayed_rf_hz: u64,
    lo_hz: u64,
    /// Digital offset the LO is placed to achieve after a retune. `0` for
    /// DC-capable backends (Pluto), `-lo_offset_hz` for zero-IF tuners
    /// (so the channel sits clear of the DC spike).
    nominal_digital_offset_hz: f64,
    dc_tunable: bool,
    dc_guard_hz: f64,
    digital_window_hz: f64,
    input_rate_hz: u32,
}

impl RadioTuner {
    /// Seed the tuner from the backend's tuning capabilities and the
    /// initial RF frequency the capture opened at. The LO is assumed
    /// placed so the channel sits at the nominal offset.
    pub fn new(tuning: &RadioTuning, initial_rf_hz: u64, input_rate_hz: u32) -> Self {
        let nominal = -tuning.lo_offset_hz;
        // LO such that displayed - lo = nominal  ⇒  lo = displayed - nominal.
        let lo = (initial_rf_hz as f64 - nominal).max(0.0) as u64;
        Self {
            displayed_rf_hz: initial_rf_hz,
            lo_hz: lo,
            nominal_digital_offset_hz: nominal,
            dc_tunable: tuning.dc_tunable,
            dc_guard_hz: tuning.dc_guard_hz,
            digital_window_hz: tuning.digital_window_hz,
            input_rate_hz,
        }
    }

    pub fn displayed_rf_hz(&self) -> u64 {
        self.displayed_rf_hz
    }
    pub fn lo_hz(&self) -> u64 {
        self.lo_hz
    }

    /// Decide how to move the receiver to `target_rf_hz` and update the
    /// internal model. Returns the command to send to the capture thread.
    pub fn tune_to(&mut self, target_rf_hz: u64) -> RadioCommand {
        let d = target_rf_hz as f64 - self.lo_hz as f64;
        let within_window = d.abs() <= self.digital_window_hz;
        let dc_ok = self.dc_tunable || d.abs() >= self.dc_guard_hz;
        if within_window && dc_ok {
            self.displayed_rf_hz = target_rf_hz;
            RadioCommand::SetDigitalOffset(d as f32)
        } else {
            self.retune_lo_to(target_rf_hz)
        }
    }

    /// Recenter the LO so the digital offset returns to nominal while the
    /// displayed frequency stays put. Useful after a chain of digital
    /// fine-tunes has walked the channel toward a band edge.
    pub fn recenter(&mut self) -> RadioCommand {
        self.retune_lo_to(self.displayed_rf_hz)
    }

    /// Place the LO so the wanted signal lands at the nominal digital
    /// offset for `target_rf_hz`, and update the model.
    fn retune_lo_to(&mut self, target_rf_hz: u64) -> RadioCommand {
        let new_lo = (target_rf_hz as f64 - self.nominal_digital_offset_hz).max(0.0) as u64;
        self.lo_hz = new_lo;
        self.displayed_rf_hz = target_rf_hz;
        RadioCommand::RetuneLo {
            lo_hz: new_lo,
            new_digital_offset_hz: self.nominal_digital_offset_hz as f32,
        }
    }

    /// Captured I/Q rate (RF spectrum span) the tuner was seeded with.
    pub fn input_rate_hz(&self) -> u32 {
        self.input_rate_hz
    }
}

/// Commands the GUI sends to the radio session (translated into backend
/// [`RadioCommand`]s and/or applied to the monitor).
#[derive(Clone, Debug)]
pub enum RadioUiCommand {
    /// Tune to an absolute RF frequency, in Hz (digital or LO, decided by
    /// the tuner).
    SetFreq(u64),
    /// Recenter the LO at the current displayed frequency.
    Recenter,
    SetGain(GainSetting),
    SetAntenna(String),
    SetDeviation(f32),
    /// Audio squelch threshold, dBFS of channel power
    /// (`f32::NEG_INFINITY` = off).
    SetSquelch(f32),
    /// Monitor playback gain (1.0 = unity).
    SetVolume(f32),
    /// Start/stop/redirect audio monitoring. `None` stops it.
    SetMonitorDevice(Option<String>),
}

/// Handle to a running radio session. Drop or [`Self::stop`] to tear it
/// down (also stops monitoring).
pub struct RadioSessionHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RadioSessionHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for RadioSessionHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Spawn the radio session thread. Returns its handle plus the audio
/// `Receiver` the RX decode worker should consume (the session forwards
/// each chunk through unchanged).
#[allow(clippy::too_many_arguments)]
pub fn spawn_session(
    audio_in: Receiver<Vec<f32>>,
    telemetry: Receiver<RadioTelemetry>,
    control: Sender<RadioCommand>,
    ui_cmds: Receiver<RadioUiCommand>,
    sink: Arc<dyn EventSink>,
    mut tuner: RadioTuner,
) -> (RadioSessionHandle, Receiver<Vec<f32>>) {
    let (audio_out, audio_rx) = mpsc::channel::<Vec<f32>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    let thread = thread::spawn(move || {
        // cpal Stream is !Send → the monitor is built and owned here.
        let mut monitor: Option<AudioMonitor> = None;

        loop {
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }

            // 1. Audio: forward to the decode worker, tee to the monitor.
            match audio_in.recv_timeout(TICK) {
                Ok(chunk) => {
                    if let Some(m) = monitor.as_ref() {
                        m.push(&chunk);
                    }
                    if audio_out.send(chunk).is_err() {
                        // Decode worker hung up — nothing left to do.
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            // 2. Telemetry → named GUI events.
            while let Ok(t) = telemetry.try_recv() {
                forward_telemetry(sink.as_ref(), t);
            }

            // 3. GUI commands → backend commands / monitor changes.
            while let Ok(cmd) = ui_cmds.try_recv() {
                apply_ui_command(cmd, &mut tuner, &control, &mut monitor);
            }
        }
    });

    (
        RadioSessionHandle {
            stop,
            thread: Some(thread),
        },
        audio_rx,
    )
}

fn forward_telemetry(sink: &dyn EventSink, t: RadioTelemetry) {
    match t {
        RadioTelemetry::RfSpectrum(frame) => sink.emit("radio_spectrum", frame),
        RadioTelemetry::AudioSpectrum(frame) => sink.emit("radio_audio_spectrum", frame),
        RadioTelemetry::SMeter {
            channel_power_dbfs,
            seq,
        } => sink.emit(
            "radio_smeter",
            serde_json::json!({ "channel_power_dbfs": channel_power_dbfs, "seq": seq }),
        ),
        RadioTelemetry::Tune(state) => sink.emit("radio_tune_state", state),
    }
}

fn apply_ui_command(
    cmd: RadioUiCommand,
    tuner: &mut RadioTuner,
    control: &Sender<RadioCommand>,
    monitor: &mut Option<AudioMonitor>,
) {
    match cmd {
        RadioUiCommand::SetFreq(hz) => {
            let _ = control.send(tuner.tune_to(hz));
        }
        RadioUiCommand::Recenter => {
            let _ = control.send(tuner.recenter());
        }
        RadioUiCommand::SetGain(g) => {
            let _ = control.send(RadioCommand::SetGain(g));
        }
        RadioUiCommand::SetAntenna(id) => {
            let _ = control.send(RadioCommand::SetAntenna(id));
        }
        RadioUiCommand::SetDeviation(dev) => {
            let _ = control.send(RadioCommand::SetDeviation(dev));
        }
        RadioUiCommand::SetSquelch(t) => {
            let _ = control.send(RadioCommand::SetSquelch(t));
        }
        RadioUiCommand::SetVolume(g) => {
            if let Some(m) = monitor.as_ref() {
                m.set_volume(g);
            }
        }
        RadioUiCommand::SetMonitorDevice(dev) => match dev {
            None => {
                *monitor = None;
            }
            Some(name) => {
                let res = match monitor.as_mut() {
                    Some(m) => m.set_device(&name),
                    None => match AudioMonitor::start(&name, MONITOR_SAMPLE_RATE) {
                        Ok(m) => {
                            *monitor = Some(m);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    },
                };
                if let Err(e) = res {
                    eprintln!("[radio] monitor device '{name}' failed: {e}");
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pluto_tuning() -> RadioTuning {
        RadioTuning {
            dc_tunable: true,
            lo_offset_hz: 0.0,
            digital_window_hz: 200_000.0,
            dc_guard_hz: 0.0,
        }
    }

    fn tuner_tuning() -> RadioTuning {
        RadioTuning {
            dc_tunable: false,
            lo_offset_hz: 75_000.0,
            digital_window_hz: 200_000.0,
            dc_guard_hz: 8_000.0,
        }
    }

    #[test]
    fn pluto_small_move_is_digital() {
        let mut t = RadioTuner::new(&pluto_tuning(), 145_500_000, 576_000);
        match t.tune_to(145_510_000) {
            RadioCommand::SetDigitalOffset(d) => assert!((d - 10_000.0).abs() < 1.0),
            other => panic!("expected digital offset, got {other:?}"),
        }
        assert_eq!(t.displayed_rf_hz(), 145_510_000);
        // LO unchanged on a digital move.
        assert_eq!(t.lo_hz(), 145_500_000);
    }

    #[test]
    fn pluto_large_move_retunes_lo() {
        let mut t = RadioTuner::new(&pluto_tuning(), 145_500_000, 576_000);
        match t.tune_to(146_000_000) {
            RadioCommand::RetuneLo {
                lo_hz,
                new_digital_offset_hz,
            } => {
                assert_eq!(lo_hz, 146_000_000);
                assert_eq!(new_digital_offset_hz, 0.0);
            }
            other => panic!("expected LO retune, got {other:?}"),
        }
        assert_eq!(t.lo_hz(), 146_000_000);
    }

    #[test]
    fn tuner_seed_keeps_channel_off_dc() {
        // Nominal offset = -75 kHz ⇒ LO sits 75 kHz above displayed.
        let t = RadioTuner::new(&tuner_tuning(), 145_500_000, 576_000);
        assert_eq!(t.lo_hz(), 145_575_000);
    }

    #[test]
    fn tuner_move_crossing_dc_forces_retune() {
        let mut t = RadioTuner::new(&tuner_tuning(), 145_500_000, 576_000);
        // Target near the LO would put d = target-lo ≈ -5 kHz, inside the
        // 8 kHz DC guard ⇒ must retune the LO instead of going digital.
        match t.tune_to(145_570_000) {
            RadioCommand::RetuneLo {
                lo_hz,
                new_digital_offset_hz,
            } => {
                // LO placed so the new offset is the nominal -75 kHz.
                assert_eq!(lo_hz, 145_645_000);
                assert!((new_digital_offset_hz - (-75_000.0)).abs() < 1.0);
            }
            other => panic!("expected LO retune across DC, got {other:?}"),
        }
    }

    #[test]
    fn tuner_in_band_move_clear_of_dc_is_digital() {
        let mut t = RadioTuner::new(&tuner_tuning(), 145_500_000, 576_000);
        // d = 145.45M - 145.575M = -125 kHz: within window, |d| > guard.
        match t.tune_to(145_450_000) {
            RadioCommand::SetDigitalOffset(d) => assert!((d - (-125_000.0)).abs() < 1.0),
            other => panic!("expected digital offset, got {other:?}"),
        }
    }

    #[test]
    fn dc_guard_boundary_is_allowed_digital() {
        let mut t = RadioTuner::new(&tuner_tuning(), 145_500_000, 576_000);
        // Choose a target so that |d| == dc_guard exactly (8 kHz).
        // lo = 145_575_000; target = lo - 8_000 = 145_567_000 ⇒ d = -8 kHz.
        match t.tune_to(145_567_000) {
            RadioCommand::SetDigitalOffset(d) => assert!((d - (-8_000.0)).abs() < 1.0),
            other => panic!("boundary |d|==guard should stay digital, got {other:?}"),
        }
    }

    #[test]
    fn recenter_keeps_displayed_freq() {
        let mut t = RadioTuner::new(&pluto_tuning(), 145_500_000, 576_000);
        // Walk digitally, then recenter.
        let _ = t.tune_to(145_650_000); // digital, d = 150 kHz
        let displayed = t.displayed_rf_hz();
        match t.recenter() {
            RadioCommand::RetuneLo { lo_hz, .. } => assert_eq!(lo_hz, displayed),
            other => panic!("expected recenter retune, got {other:?}"),
        }
        assert_eq!(t.displayed_rf_hz(), displayed);
        assert_eq!(t.lo_hz(), displayed);
    }
}
