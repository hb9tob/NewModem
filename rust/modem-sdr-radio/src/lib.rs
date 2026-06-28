//! Backend-agnostic Radio-tab runtime.
//!
//! Every SDR backend (Pluto, SDRplay, RTL-SDR) drives the GUI Radio tab
//! through one shared [`RadioRuntime`]: it computes the wideband RF
//! spectrum (from the raw I/Q), the demodulated-audio spectrum and the
//! S-meter (from the chain's audio + channel power), emits them as
//! [`RadioTelemetry`], and applies hybrid digital-NCO / hardware-LO tune
//! commands. The DSP and telemetry are identical across backends; only the
//! hardware-specific LO / gain writes differ, so those are delegated to the
//! backend through the [`RadioHardware`] trait.
//!
//! Frequency model. The wideband FFT is taken on the raw I/Q, physically
//! centred on the hardware LO, so the RF [`SpectrumFrame`] is stamped with
//! `center_hz = lo_hz`. The operator listens at
//! `displayed_rf = lo_hz + digital_offset_hz`. On a zero-IF backend the LO
//! is programmed `lo_offset` above the user frequency (to dodge the DC
//! spike) and the chain's NCO sits at `-lo_offset`; the runtime is seeded
//! with `digital_offset_hz = -lo_offset` so `displayed_rf` equals the user
//! frequency. Pluto has `lo_offset = 0`, so the runtime behaves exactly as
//! its former private copy did.
//!
//! Threading. The runtime is `Send` but not internally synchronised: each
//! backend owns it (behind its own mutex for callback-driven backends) and
//! decides where to call [`RadioRuntime::on_iq`] / [`on_audio`](RadioRuntime::on_audio)
//! (in the sample callback) and [`apply_command`](RadioRuntime::apply_command)
//! (on a thread that may safely touch the hardware — never the USB/daemon
//! callback thread).

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use num_complex::Complex32;

use modem_sdr::config::GainSetting;
use modem_sdr::telemetry::{DemodMode, RadioCommand, RadioTelemetry, SpectrumFrame, TuneState};
use modem_sdr_dsp::{NbfmRxChainConfig, RxChain, SpectrumAnalyzer, SsbRxChainConfig, AUDIO_RATE};

/// FFT size for the wideband RF spectrum / waterfall. 8192 bins over a
/// 576 kHz span ≈ 70 Hz/bin — fine enough for the SSB fine-tuning zoom (the
/// GUI shows a ±10 kHz slice) while still cheap at the 80 ms emit cadence.
const RF_FFT_SIZE: usize = 8192;

/// FFT size for the demodulated-audio spectrum. 4096 over the 0–24 kHz
/// half-band ≈ 11.7 Hz/bin — fine enough to resolve the voice-band detail in
/// the 0–4 kHz zoom the Radio tab shows.
const AUDIO_FFT_SIZE: usize = 4096;

/// Minimum spacing between successive RF spectrum frames (~12 Hz).
const RF_FRAME_PERIOD: Duration = Duration::from_millis(80);
/// Minimum spacing between successive audio spectrum frames (~15 Hz).
const AUDIO_FRAME_PERIOD: Duration = Duration::from_millis(66);
/// Minimum spacing between successive S-meter frames (~10 Hz).
const SMETER_PERIOD: Duration = Duration::from_millis(100);
/// Minimum spacing between successive FM-excursion frames (~20 Hz).
/// Faster than the spectra so brief over-deviation transients still show;
/// the peak is held between emits so none is missed even at this cadence.
const EXCURSION_PERIOD: Duration = Duration::from_millis(50);

/// Backend-specific live hardware writes the runtime delegates to. Calls
/// are best-effort: implementations log their own failures and return,
/// because they run in a context (sample-callback-adjacent / supervisor
/// loop) with no caller to propagate an error to. Operator-driven and
/// rare, so the per-call cost is irrelevant.
pub trait RadioHardware {
    /// Program the hardware LO to `lo_hz` (the already-offset, programmed
    /// LO — the runtime adds no further offset).
    fn retune_lo(&mut self, lo_hz: u64);
    /// Apply a new gain setting live.
    fn set_gain(&mut self, gain: &GainSetting);
    /// Select a new antenna by capability id. No-op on single-port backends.
    fn set_antenna(&mut self, _id: &str) {}
}

/// Seed values for [`RadioRuntime::new`], gathered by the backend at
/// capture start.
#[derive(Clone, Copy, Debug)]
pub struct RadioInit {
    /// Captured I/Q sample rate (= RF spectrum span), Hz.
    pub input_rate_hz: u32,
    /// Programmed hardware LO at start, Hz (`user + lo_offset`).
    pub lo_hz: u64,
    /// Initial digital NCO offset, Hz (`displayed_rf - lo`). `0` for
    /// dc-tunable backends, `-lo_offset` for zero-IF ones.
    pub digital_offset_hz: f64,
    /// Deliberate LO offset baked into the chain, Hz. Reused when the
    /// channel filter is rebuilt on a `SetDeviation`. `0` on Pluto.
    pub lo_offset_hz: f32,
    /// Current max FM deviation, Hz (rebuild parameter).
    pub max_deviation_hz: f32,
    /// Whether the backend can tune through DC (Pluto true, zero-IF false).
    pub dc_tunable: bool,
    /// Active demodulation mode at capture start. Default `Nbfm`.
    pub demod_mode: DemodMode,
    /// SSB channel bandwidth, Hz (rebuild parameter, used in `SsbUsb`).
    pub ssb_bandwidth_hz: f32,
}

/// The shared Radio-tab runtime. One per active SDR capture.
pub struct RadioRuntime {
    telemetry_tx: Sender<RadioTelemetry>,

    rf_analyzer: SpectrumAnalyzer,
    audio_analyzer: SpectrumAnalyzer,
    rf_bins: Vec<f32>,
    audio_bins: Vec<f32>,
    /// Ring of the most recent raw I/Q samples, capped at [`RF_FFT_SIZE`].
    /// The sample-callback chunk size varies per backend and may be smaller
    /// than the FFT (SDRplay delivers ~1k/callback), so we accumulate
    /// rather than assume one chunk fills a frame.
    rf_accum: Vec<Complex32>,
    /// Ring of the most recent audio samples, capped at [`AUDIO_FFT_SIZE`].
    audio_accum: Vec<f32>,

    input_rate_hz: u32,
    lo_hz: u64,
    digital_offset_hz: f64,
    lo_offset_hz: f32,
    max_deviation_hz: f32,
    dc_tunable: bool,
    /// Active demodulation mode — selects which chain `apply_command`
    /// rebuilds and how `on_audio` tags the level meter.
    demod_mode: DemodMode,
    /// SSB channel bandwidth, Hz (rebuild parameter for `SsbUsb`).
    ssb_bandwidth_hz: f32,
    /// Audio squelch threshold in dBFS of channel power.
    /// `f32::NEG_INFINITY` = squelch off.
    squelch_dbfs: f32,

    rf_seq: u64,
    audio_seq: u64,
    smeter_seq: u64,
    excursion_seq: u64,
    last_rf: Instant,
    last_audio: Instant,
    last_smeter: Instant,
    last_excursion: Instant,
    /// Peak-hold of the normalised excursion peak / RMS accumulated since
    /// the last emitted FM-excursion frame, so a transient between emits is
    /// not lost. Reset to 0 on each emit.
    exc_peak_hold: f32,
    exc_rms_hold: f32,
}

impl RadioRuntime {
    /// Build the runtime and emit the initial [`TuneState`].
    pub fn new(telemetry_tx: Sender<RadioTelemetry>, init: RadioInit) -> Self {
        let now = Instant::now();
        let rt = Self {
            telemetry_tx,
            rf_analyzer: SpectrumAnalyzer::new(RF_FFT_SIZE),
            audio_analyzer: SpectrumAnalyzer::new(AUDIO_FFT_SIZE),
            rf_bins: Vec::with_capacity(RF_FFT_SIZE),
            audio_bins: Vec::with_capacity(AUDIO_FFT_SIZE / 2),
            rf_accum: Vec::with_capacity(RF_FFT_SIZE),
            audio_accum: Vec::with_capacity(AUDIO_FFT_SIZE),
            input_rate_hz: init.input_rate_hz,
            lo_hz: init.lo_hz,
            digital_offset_hz: init.digital_offset_hz,
            lo_offset_hz: init.lo_offset_hz,
            max_deviation_hz: init.max_deviation_hz,
            dc_tunable: init.dc_tunable,
            demod_mode: init.demod_mode,
            ssb_bandwidth_hz: init.ssb_bandwidth_hz,
            squelch_dbfs: f32::NEG_INFINITY,
            rf_seq: 0,
            audio_seq: 0,
            smeter_seq: 0,
            excursion_seq: 0,
            last_rf: now,
            last_audio: now,
            last_smeter: now,
            last_excursion: now,
            exc_peak_hold: 0.0,
            exc_rms_hold: 0.0,
        };
        rt.send_tune();
        rt
    }

    /// Frequency the operator is actually listening to: `lo + offset`.
    pub fn displayed_rf_hz(&self) -> u64 {
        (self.lo_hz as f64 + self.digital_offset_hz).max(0.0) as u64
    }

    /// Programmed hardware LO, Hz (centre of the RF spectrum).
    pub fn lo_hz(&self) -> u64 {
        self.lo_hz
    }

    fn send_tune(&self) {
        let _ = self.telemetry_tx.send(RadioTelemetry::Tune(TuneState {
            displayed_rf_hz: self.displayed_rf_hz(),
            lo_hz: self.lo_hz,
            digital_offset_hz: self.digital_offset_hz,
            input_rate_hz: self.input_rate_hz,
            dc_tunable: self.dc_tunable,
        }));
    }

    /// Apply the **hardware** side of a command (LO / gain / antenna). This
    /// is split out from [`apply_command`](Self::apply_command) and **must
    /// be called outside any lock the sample callback also takes**: the FFI
    /// write can block, and on callback-driven backends (RTL-SDR) the USB
    /// event thread must keep running to complete it — holding the callback
    /// lock would deadlock the stream. Stateless (no `self`).
    pub fn run_hardware(cmd: &RadioCommand, hw: &mut dyn RadioHardware) {
        match cmd {
            RadioCommand::RetuneLo { lo_hz, .. } => hw.retune_lo(*lo_hz),
            RadioCommand::SetGain(g) => hw.set_gain(g),
            RadioCommand::SetAntenna(id) => hw.set_antenna(id),
            _ => {}
        }
    }

    /// Apply the **DSP / telemetry** side of a command (chain + tuning
    /// state). Run this under the sample-callback lock. For a command with a
    /// hardware effect (RetuneLo / SetGain / SetAntenna), call
    /// [`run_hardware`](Self::run_hardware) first, outside the lock.
    pub fn apply_command(&mut self, cmd: RadioCommand, chain: &mut RxChain) {
        match cmd {
            RadioCommand::SetDigitalOffset(d) => {
                chain.set_channel_freq(d);
                self.digital_offset_hz = d as f64;
                self.send_tune();
            }
            RadioCommand::RetuneLo {
                lo_hz,
                new_digital_offset_hz,
            } => {
                // The LO was already programmed by `run_hardware`. The old
                // chain state (FIR history, NCO phase, IIR) is now stale;
                // reset before re-applying the offset.
                chain.reset();
                chain.set_channel_freq(new_digital_offset_hz);
                self.lo_hz = lo_hz;
                self.digital_offset_hz = new_digital_offset_hz as f64;
                self.send_tune();
            }
            // Hardware-only — handled by `run_hardware`, no DSP state change.
            RadioCommand::SetGain(_) | RadioCommand::SetAntenna(_) => {}
            RadioCommand::SetDeviation(dev) => {
                // Rebuild the channel filter / discriminator for the new
                // deviation, reusing this backend's lo_offset, then restore
                // the current digital tune (set_channel_freq overrides the
                // construction-time NCO centre). Deviation is an NBFM-only
                // control (the GUI only surfaces it in NBFM mode), so this
                // also lands the chain in NBFM.
                *chain = RxChain::nbfm(NbfmRxChainConfig::new(
                    self.input_rate_hz,
                    dev,
                    self.lo_offset_hz,
                ));
                chain.set_channel_freq(self.digital_offset_hz as f32);
                self.max_deviation_hz = dev;
                self.demod_mode = DemodMode::Nbfm;
            }
            RadioCommand::SetSquelch(t) => {
                self.squelch_dbfs = t;
            }
            RadioCommand::SetDemodMode(mode) => {
                // Rebuild as the selected chain (mirrors SetDeviation's
                // rebuild), reusing this backend's lo_offset, then restore
                // the current digital tune.
                self.demod_mode = mode;
                *chain = match mode {
                    DemodMode::Nbfm => RxChain::nbfm(NbfmRxChainConfig::new(
                        self.input_rate_hz,
                        self.max_deviation_hz,
                        self.lo_offset_hz,
                    )),
                    DemodMode::SsbUsb => RxChain::ssb(SsbRxChainConfig::new(
                        self.input_rate_hz,
                        self.ssb_bandwidth_hz,
                        self.lo_offset_hz,
                    )),
                };
                chain.set_channel_freq(self.digital_offset_hz as f32);
            }
            RadioCommand::SetSsbBandwidth(bw) => {
                self.ssb_bandwidth_hz = bw;
                // Only rebuilds while in SSB; a no-op (but remembered) in NBFM.
                if self.demod_mode == DemodMode::SsbUsb {
                    *chain = RxChain::ssb(SsbRxChainConfig::new(
                        self.input_rate_hz,
                        bw,
                        self.lo_offset_hz,
                    ));
                    chain.set_channel_freq(self.digital_offset_hz as f32);
                }
            }
        }
    }

    /// Wideband RF spectrum from the raw I/Q, throttled to
    /// [`RF_FRAME_PERIOD`]. `iq` is the raw capture (pre-chain), physically
    /// centred on the LO.
    pub fn on_iq(&mut self, iq: &[Complex32]) {
        // Keep a ring of the most recent RF_FFT_SIZE raw samples so the RF
        // spectrum works whatever the per-callback chunk size.
        self.rf_accum.extend_from_slice(iq);
        if self.rf_accum.len() > RF_FFT_SIZE {
            let drop = self.rf_accum.len() - RF_FFT_SIZE;
            self.rf_accum.drain(..drop);
        }
        if self.rf_accum.len() < RF_FFT_SIZE || self.last_rf.elapsed() < RF_FRAME_PERIOD {
            return;
        }
        self.rf_analyzer
            .process_complex(&self.rf_accum, &mut self.rf_bins);
        self.rf_seq += 1;
        let _ = self
            .telemetry_tx
            .send(RadioTelemetry::RfSpectrum(SpectrumFrame {
                bins_db: self.rf_bins.clone(),
                center_hz: self.lo_hz as f64,
                span_hz: self.input_rate_hz as f64,
                seq: self.rf_seq,
            }));
        self.last_rf = Instant::now();
    }

    /// S-meter + audio spectrum + level meter from the demodulated audio.
    /// `channel_power_lin` is the chain's last linear channel power;
    /// `meter_peak` / `meter_rms` are the chain's last meter peak / RMS
    /// (`RxChain::last_meter_peak`/`_rms`) — normalised FM excursion in NBFM
    /// (`1.0` == max deviation), linear analytic envelope in SSB. The frame
    /// is tagged accordingly (`FmExcursion` vs `AudioLevel`). Returns `true`
    /// when the chunk should be muted (channel power below the squelch
    /// threshold).
    pub fn on_audio(
        &mut self,
        audio: &[f32],
        channel_power_lin: f32,
        meter_peak: f32,
        meter_rms: f32,
    ) -> bool {
        let power_dbfs = if channel_power_lin > 0.0 {
            10.0 * channel_power_lin.log10()
        } else {
            -140.0
        };

        if self.last_smeter.elapsed() >= SMETER_PERIOD {
            self.smeter_seq += 1;
            let _ = self.telemetry_tx.send(RadioTelemetry::SMeter {
                channel_power_dbfs: power_dbfs,
                seq: self.smeter_seq,
            });
            self.last_smeter = Instant::now();
        }

        // Level meter (Radio-tab over-modulation / SSB level display).
        // Peak-hold the raw meter values across callbacks so a brief transient
        // between emits is not missed, then tag the frame by mode: NBFM scales
        // the normalised excursion by max deviation to report absolute Hz; SSB
        // reports the linear analytic envelope as-is.
        self.exc_peak_hold = self.exc_peak_hold.max(meter_peak);
        self.exc_rms_hold = self.exc_rms_hold.max(meter_rms);
        if self.last_excursion.elapsed() >= EXCURSION_PERIOD {
            self.excursion_seq += 1;
            let frame = match self.demod_mode {
                DemodMode::Nbfm => RadioTelemetry::FmExcursion {
                    peak_hz: self.exc_peak_hold * self.max_deviation_hz,
                    rms_hz: self.exc_rms_hold * self.max_deviation_hz,
                    max_dev_hz: self.max_deviation_hz,
                    seq: self.excursion_seq,
                },
                DemodMode::SsbUsb => RadioTelemetry::AudioLevel {
                    peak: self.exc_peak_hold,
                    rms: self.exc_rms_hold,
                    seq: self.excursion_seq,
                },
            };
            let _ = self.telemetry_tx.send(frame);
            self.exc_peak_hold = 0.0;
            self.exc_rms_hold = 0.0;
            self.last_excursion = Instant::now();
        }

        // Keep a ring of the most recent AUDIO_FFT_SIZE samples.
        self.audio_accum.extend_from_slice(audio);
        if self.audio_accum.len() > AUDIO_FFT_SIZE {
            let drop = self.audio_accum.len() - AUDIO_FFT_SIZE;
            self.audio_accum.drain(..drop);
        }
        if self.audio_accum.len() == AUDIO_FFT_SIZE
            && self.last_audio.elapsed() >= AUDIO_FRAME_PERIOD
        {
            self.audio_analyzer
                .process_real(&self.audio_accum, &mut self.audio_bins);
            self.audio_seq += 1;
            let _ = self
                .telemetry_tx
                .send(RadioTelemetry::AudioSpectrum(SpectrumFrame {
                    bins_db: self.audio_bins.clone(),
                    center_hz: (AUDIO_RATE as f64) / 4.0,
                    span_hz: (AUDIO_RATE as f64) / 2.0,
                    seq: self.audio_seq,
                }));
            self.last_audio = Instant::now();
        }

        self.squelch_dbfs.is_finite() && power_dbfs < self.squelch_dbfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Records the hardware calls the runtime delegates.
    #[derive(Default)]
    struct MockHw {
        last_lo: Option<u64>,
        gain_calls: u32,
    }
    impl RadioHardware for MockHw {
        fn retune_lo(&mut self, lo_hz: u64) {
            self.last_lo = Some(lo_hz);
        }
        fn set_gain(&mut self, _gain: &GainSetting) {
            self.gain_calls += 1;
        }
    }

    fn chain() -> RxChain {
        RxChain::nbfm(NbfmRxChainConfig::new(576_000, 5_000.0, 0.0))
    }

    fn drain_tune(rx: &mpsc::Receiver<RadioTelemetry>) -> Option<TuneState> {
        let mut last = None;
        while let Ok(t) = rx.try_recv() {
            if let RadioTelemetry::Tune(s) = t {
                last = Some(s);
            }
        }
        last
    }

    #[test]
    fn pluto_seed_displays_user_freq() {
        let (tx, rx) = mpsc::channel();
        let rt = RadioRuntime::new(
            tx,
            RadioInit {
                input_rate_hz: 576_000,
                lo_hz: 145_500_000,
                digital_offset_hz: 0.0,
                lo_offset_hz: 0.0,
                max_deviation_hz: 5_000.0,
                dc_tunable: true,
                demod_mode: DemodMode::Nbfm,
                ssb_bandwidth_hz: 2_700.0,
            },
        );
        assert_eq!(rt.displayed_rf_hz(), 145_500_000);
        let s = drain_tune(&rx).expect("initial tune");
        assert_eq!(s.lo_hz, 145_500_000);
        assert!(s.dc_tunable);
    }

    #[test]
    fn zero_if_seed_offsets_so_displayed_equals_user() {
        // SDRplay: LO programmed 75 kHz above user, NCO at -75 kHz.
        let (tx, rx) = mpsc::channel();
        let rt = RadioRuntime::new(
            tx,
            RadioInit {
                input_rate_hz: 576_000,
                lo_hz: 145_575_000,
                digital_offset_hz: -75_000.0,
                lo_offset_hz: 75_000.0,
                max_deviation_hz: 5_000.0,
                dc_tunable: false,
                demod_mode: DemodMode::Nbfm,
                ssb_bandwidth_hz: 2_700.0,
            },
        );
        assert_eq!(rt.displayed_rf_hz(), 145_500_000);
        let s = drain_tune(&rx).expect("initial tune");
        assert!(!s.dc_tunable);
        assert_eq!(s.lo_hz, 145_575_000);
    }

    #[test]
    fn digital_offset_moves_displayed_not_lo() {
        let (tx, rx) = mpsc::channel();
        let mut rt = RadioRuntime::new(
            tx,
            RadioInit {
                input_rate_hz: 576_000,
                lo_hz: 145_500_000,
                digital_offset_hz: 0.0,
                lo_offset_hz: 0.0,
                max_deviation_hz: 5_000.0,
                dc_tunable: true,
                demod_mode: DemodMode::Nbfm,
                ssb_bandwidth_hz: 2_700.0,
            },
        );
        let mut hw = MockHw::default();
        let mut ch = chain();
        let cmd = RadioCommand::SetDigitalOffset(10_000.0);
        RadioRuntime::run_hardware(&cmd, &mut hw);
        rt.apply_command(cmd, &mut ch);
        assert_eq!(rt.lo_hz(), 145_500_000); // LO untouched
        assert_eq!(rt.displayed_rf_hz(), 145_510_000);
        assert!(hw.last_lo.is_none()); // no hardware retune
        let s = drain_tune(&rx).expect("tune after digital move");
        assert_eq!(s.displayed_rf_hz, 145_510_000);
    }

    #[test]
    fn set_demod_mode_rebuilds_working_ssb_chain() {
        // After SetDemodMode(SsbUsb) the chain must be an SSB chain that
        // actually demodulates: feed a +1 kHz USB tone (carrier at DC) and
        // confirm audio comes out. SetSsbBandwidth before the switch must be
        // remembered and applied at rebuild.
        let (tx, _rx) = mpsc::channel();
        let mut rt = RadioRuntime::new(
            tx,
            RadioInit {
                input_rate_hz: 576_000,
                lo_hz: 145_500_000,
                digital_offset_hz: 0.0,
                lo_offset_hz: 0.0,
                max_deviation_hz: 5_000.0,
                dc_tunable: true,
                demod_mode: DemodMode::Nbfm,
                ssb_bandwidth_hz: 2_700.0,
            },
        );
        let mut ch = chain();
        rt.apply_command(RadioCommand::SetSsbBandwidth(2_400.0), &mut ch);
        rt.apply_command(RadioCommand::SetDemodMode(DemodMode::SsbUsb), &mut ch);

        use std::f32::consts::PI;
        let fs = 576_000.0_f32;
        let iq: Vec<Complex32> = (0..576_000)
            .map(|k| {
                let phi = 2.0 * PI * 1_000.0 * k as f32 / fs;
                Complex32::new(phi.cos(), phi.sin())
            })
            .collect();
        let audio = ch.process(&iq);
        let skip = 4_000usize;
        let rms: f32 = (audio.iter().skip(skip).map(|s| s * s).sum::<f32>()
            / (audio.len() - skip) as f32)
            .sqrt();
        assert!(rms > 0.05, "SSB chain after mode switch produced no audio (rms={rms})");
        assert!(audio.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn retune_lo_calls_hardware_and_updates_state() {
        let (tx, rx) = mpsc::channel();
        let mut rt = RadioRuntime::new(
            tx,
            RadioInit {
                input_rate_hz: 576_000,
                lo_hz: 145_500_000,
                digital_offset_hz: 0.0,
                lo_offset_hz: 0.0,
                max_deviation_hz: 5_000.0,
                dc_tunable: true,
                demod_mode: DemodMode::Nbfm,
                ssb_bandwidth_hz: 2_700.0,
            },
        );
        let mut hw = MockHw::default();
        let mut ch = chain();
        let cmd = RadioCommand::RetuneLo {
            lo_hz: 146_000_000,
            new_digital_offset_hz: 0.0,
        };
        RadioRuntime::run_hardware(&cmd, &mut hw);
        rt.apply_command(cmd, &mut ch);
        assert_eq!(hw.last_lo, Some(146_000_000));
        assert_eq!(rt.lo_hz(), 146_000_000);
        assert_eq!(rt.displayed_rf_hz(), 146_000_000);
        let s = drain_tune(&rx).expect("tune after retune");
        assert_eq!(s.lo_hz, 146_000_000);
    }
}
