//! Canonical SSB (USB-data) SDR RX chain — the linear-modulation
//! counterpart of [`NbfmRxChain`](crate::nbfm_rx_chain::NbfmRxChain),
//! shared by every backend the same way.
//!
//! Built for the QO-100 ground rehearsal: receive a raw xAPSK modem
//! signal transmitted as USB-data SSB (no FM carrier), and hand the
//! existing modem RX **exactly the real audio it already expects** — the
//! data sitting on its native audio carrier (1100 Hz today). That makes
//! the SDR front-end and a real SSB transceiver's AF output interchange-
//! able: one decoder, two front-ends.
//!
//! ```text
//! I/Q at input_rate_hz
//!   → FreqXlatingFir(decim, kaiser_lpf, lo_offset_hz, input_rate_hz)
//!         ↪ NCO brings the suppressed carrier f_sc down to DC
//!         ↪ LPF rejects adjacent channels, decimates to AUDIO_RATE
//!   → ComplexFir(analytic_bandpass)  [keeps +[f_lo, ssb_bw], rejects image]
//!   → Re{·}                          [→ real audio, wanted sideband only]
//!   → SubAudioHpf (300 Hz)           [kills carrier / DC residual]
//!   → 48 kHz f32 mono audio (data at its native ~1100 Hz)
//! ```
//!
//! Deliberately **no de-emphasis** (SSB carries none) — that's the only
//! structural difference from the NBFM chain besides the demod stage.
//! `NbfmRxChain` and its blocks are not touched; this is an additive,
//! parallel chain selected by [`RxChain`](crate::rx_chain::RxChain).

use num_complex::Complex32;

use crate::audio_filters::SubAudioHpf;
use crate::complex_fir::{analytic_bandpass_taps, ComplexFir, Sideband};
use crate::decimator::kaiser_sinc_taps;
use crate::freq_xlating::FreqXlatingFir;
use crate::nbfm_rx_chain::{AUDIO_MARGIN_HZ, DEFAULT_STOPBAND_DB, TRANSITION_RATIO};
use crate::AUDIO_RATE;

/// Lower edge of the analytic band-pass, in Hz. Kept low (the 300 Hz
/// `SubAudioHpf` does the final carrier/DC cleanup); the analytic
/// filter's real job is rejecting the negative-frequency image, so this
/// only has to clear the suppressed-carrier line at DC.
pub const DEFAULT_AUDIO_LOW_EDGE_HZ: f32 = 100.0;

/// Digital-AGC target for the analytic envelope `|z|`. SSB is a linear
/// mode: unlike NBFM (whose discriminator is amplitude-independent and
/// lands the audio near full scale on its own), the SSB audio level just
/// follows the received amplitude, so without normalisation a typical
/// off-air signal comes out tens of dB low. This AGC normalises the mean
/// envelope to a comfortable level on TOP of whatever the SDR's own gain
/// /AGC delivers. 0.35 → audio ≈ −9 dBFS RMS with headroom for the APSK
/// peak-to-average and RRC overshoot.
pub const AGC_TARGET_ENV: f32 = 0.35;

/// Maximum digital-AGC make-up gain (linear). +24 dB lifts a weak signal
/// without amplifying inter-burst noise — or a burst onset before the
/// loop settles — to hard clipping (a strong burst needs little make-up
/// anyway). The SDR's hardware gain does the bulk of the work; this is
/// the residual normaliser.
pub const AGC_MAX_GAIN: f32 = 16.0;

/// AGC attack time constant (s) — how fast the gain comes DOWN when the
/// envelope rises. 50 ms settles well within the 256-symbol preamble at
/// every profile (≥ 150 ms), so the data after the preamble sees a stable
/// gain and the APSK amplitude rings are preserved.
pub const AGC_ATTACK_S: f32 = 0.050;

/// AGC release time constant (s) — how slowly the gain comes UP when the
/// envelope falls. 300 ms holds the gain steady across a burst and across
/// the short inter-frame silences (so a following burst's onset isn't
/// over-amplified), yet lets a fresh weak signal come up to level within
/// ~1 s. Still far slower than the symbol rate, so APSK amplitude rings
/// are untouched (the decoder also re-normalises).
pub const AGC_RELEASE_S: f32 = 0.3;

/// Configuration for [`SsbRxChain`]. Mirrors
/// [`NbfmRxChainConfig`](crate::nbfm_rx_chain::NbfmRxChainConfig) so the
/// two chains plug into the same orchestration.
#[derive(Debug, Clone)]
pub struct SsbRxChainConfig {
    /// Input I/Q rate, Hz. Must be an integer multiple of [`AUDIO_RATE`]
    /// (same constraint as the NBFM chain).
    pub input_rate_hz: u32,
    /// SSB channel bandwidth, Hz — the upper edge of the wanted sideband
    /// (`f_hi` of the analytic band-pass). Target set 2200 / 2400 / 2700;
    /// default 2700 (QO-100 allows 2.7 kHz anywhere).
    pub ssb_bandwidth_hz: f32,
    /// LO-offset compensation, Hz. **Same sign convention as the NBFM
    /// chain**: positive when the hardware LO was programmed above the
    /// user's target; the NCO multiplies by `exp(+j·2π·lo_offset·n/fs)`
    /// to bring the wanted signal back to DC. 0 on Pluto (hardware DC
    /// comp), +75 000 on SDRplay, +250 000 on RTL-SDR.
    pub lo_offset_hz: f32,
    /// Which sideband to keep. `Usb` today.
    pub sideband: Sideband,
    /// Lower edge of the analytic band-pass, Hz. [`DEFAULT_AUDIO_LOW_EDGE_HZ`].
    pub audio_low_edge_hz: f32,
    /// Channel + image-rejection stop-band depth, dB. [`DEFAULT_STOPBAND_DB`].
    pub stopband_db: f32,
    /// Enable the digital AGC (level normaliser) on the output audio.
    /// **Default `false`.** The modem decoder already normalises each
    /// burst's level internally (the FFE), and an external AGC's onset
    /// ramp fights that LS fit and breaks decoding — so the AGC is a
    /// **monitor/listening** aid, not for the decode path, and stays off
    /// until wired to the monitor tee. The meter is pre-AGC (true level)
    /// regardless. See [`AGC_TARGET_ENV`].
    pub agc: bool,
}

impl SsbRxChainConfig {
    /// Build a USB config with the defaults filled in. The caller picks
    /// the rate, bandwidth and LO offset (backend-dependent).
    pub fn new(input_rate_hz: u32, ssb_bandwidth_hz: f32, lo_offset_hz: f32) -> Self {
        Self {
            input_rate_hz,
            ssb_bandwidth_hz,
            lo_offset_hz,
            sideband: Sideband::Usb,
            audio_low_edge_hz: DEFAULT_AUDIO_LOW_EDGE_HZ,
            stopband_db: DEFAULT_STOPBAND_DB,
            agc: false,
        }
    }
}

/// Stateful SSB RX chain. Same lifecycle as
/// [`NbfmRxChain`](crate::nbfm_rx_chain::NbfmRxChain): construct once,
/// feed `Complex32` chunks via [`Self::process`] for 48 kHz mono f32
/// audio. Internal state crosses chunk boundaries.
pub struct SsbRxChain {
    /// Channel-select: NCO + LPF + decim from `input_rate_hz` to
    /// AUDIO_RATE. Identical block + sign convention as the NBFM chain.
    xlating: FreqXlatingFir,
    /// One-sided analytic band-pass at audio rate — selects the wanted
    /// sideband, rejects the image. This is the SSB-specific stage.
    analytic: ComplexFir,
    /// CTCSS/DC reject — kills the suppressed-carrier line and any DC
    /// residual. Reused verbatim from the NBFM chain.
    hpf: SubAudioHpf,
    /// Scratch for the real-part audio so `process` doesn't allocate it
    /// per call.
    audio_scratch: Vec<f32>,
    /// Cached input rate for [`Self::set_channel_freq`].
    input_rate_hz: f32,
    /// Mean `|baseband|²` of the last `process` — the channel-selected
    /// power feeding the S-meter (identical semantics to the NBFM chain).
    last_channel_power: f32,
    /// Peak analytic-envelope `|z|` of the last `process` — a **linear**
    /// level (the FM excursion meter is meaningless for SSB). Read-only
    /// side-channel for the Radio-tab level meter. **Pre-AGC** (the true
    /// received level), so the meter still reads signal strength.
    last_level_peak: f32,
    /// RMS analytic-envelope of the last `process`.
    last_level_rms: f32,
    /// Digital AGC enabled.
    agc: bool,
    /// Smoothed envelope estimate the AGC gain is derived from. Carried
    /// across chunks (fast-attack / slow-release follower).
    agc_env: f32,
    /// One-pole attack coefficient (rising envelope).
    agc_attack: f32,
    /// One-pole release coefficient (falling envelope).
    agc_release: f32,
}

impl SsbRxChain {
    /// Build the chain. Panics if `input_rate_hz` isn't a positive
    /// integer multiple of [`AUDIO_RATE`].
    pub fn new(cfg: SsbRxChainConfig) -> Self {
        assert!(
            cfg.input_rate_hz > 0 && cfg.input_rate_hz % AUDIO_RATE == 0,
            "input_rate_hz {} must be a positive integer multiple of AUDIO_RATE {}",
            cfg.input_rate_hz,
            AUDIO_RATE
        );
        assert!(cfg.ssb_bandwidth_hz > 0.0, "ssb_bandwidth_hz must be positive");
        assert!(
            cfg.audio_low_edge_hz >= 0.0 && cfg.audio_low_edge_hz < cfg.ssb_bandwidth_hz,
            "audio_low_edge_hz {} must be in [0, ssb_bandwidth_hz {})",
            cfg.audio_low_edge_hz,
            cfg.ssb_bandwidth_hz
        );
        assert!(cfg.stopband_db > 0.0, "stopband_db must be positive");
        let decim = (cfg.input_rate_hz / AUDIO_RATE) as usize;

        // Stage-1 channel filter, sized like the NBFM chain (the sharp
        // sideband selection is done cheaply at audio rate downstream, so
        // this just has to pass the wanted band and anti-alias the decim).
        let passband_end = cfg.ssb_bandwidth_hz + AUDIO_MARGIN_HZ;
        let stopband_start = passband_end * TRANSITION_RATIO;
        let taps = kaiser_sinc_taps(
            cfg.input_rate_hz as f32,
            passband_end,
            stopband_start,
            cfg.stopband_db,
        );
        // Same sign flip as nbfm_rx_chain: `lo_offset_hz` is "I offset my
        // LO upward", so the wanted carrier sits at `−lo_offset_hz`.
        let xlating = FreqXlatingFir::new(decim, taps, -cfg.lo_offset_hz, cfg.input_rate_hz as f32);

        let analytic = ComplexFir::new(analytic_bandpass_taps(
            AUDIO_RATE as f32,
            cfg.audio_low_edge_hz,
            cfg.ssb_bandwidth_hz,
            cfg.sideband,
            cfg.stopband_db,
        ));
        let hpf = SubAudioHpf::calibrated(AUDIO_RATE as f32);

        Self {
            xlating,
            analytic,
            hpf,
            audio_scratch: Vec::new(),
            input_rate_hz: cfg.input_rate_hz as f32,
            last_channel_power: 0.0,
            last_level_peak: 0.0,
            last_level_rms: 0.0,
            agc: cfg.agc,
            // Start at the target so the initial gain is unity (neutral) and
            // a first burst ramps gently, never a sudden boost.
            agc_env: AGC_TARGET_ENV,
            agc_attack: 1.0 - (-1.0 / (AGC_ATTACK_S * AUDIO_RATE as f32)).exp(),
            agc_release: 1.0 - (-1.0 / (AGC_RELEASE_S * AUDIO_RATE as f32)).exp(),
        }
    }

    /// Live-retune the channel-select NCO (digital DDC fine-tune), same
    /// semantics as [`NbfmRxChain::set_channel_freq`](crate::nbfm_rx_chain::NbfmRxChain::set_channel_freq).
    #[inline]
    pub fn set_channel_freq(&mut self, center_hz: f32) {
        self.xlating.set_center_freq(center_hz, self.input_rate_hz);
    }

    /// Mean `|baseband|²` of the last `process` — drives the S-meter.
    #[inline]
    pub fn last_channel_power(&self) -> f32 {
        self.last_channel_power
    }

    /// Peak analytic envelope of the last `process` — a linear level for
    /// the Radio-tab meter (unlike NBFM there is no deviation to report).
    #[inline]
    pub fn last_level_peak(&self) -> f32 {
        self.last_level_peak
    }

    /// RMS analytic envelope of the last `process`.
    #[inline]
    pub fn last_level_rms(&self) -> f32 {
        self.last_level_rms
    }

    /// Input I/Q sample rate, Hz.
    #[inline]
    pub fn input_rate_hz(&self) -> f32 {
        self.input_rate_hz
    }

    /// Channel-select FIR tap count (for session-start logging).
    #[inline]
    pub fn channel_filter_taps(&self) -> usize {
        self.xlating.num_taps()
    }

    /// Decimation factor of the channel-select stage.
    #[inline]
    pub fn decimation_factor(&self) -> usize {
        self.xlating.factor()
    }

    /// Process a chunk of complex baseband, returning 48 kHz audio.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<f32> {
        // Stage 1: channel select → 48 kHz complex baseband (f_sc at DC).
        let baseband = self.xlating.process(iq);
        if baseband.is_empty() {
            return Vec::new();
        }

        // S-meter: mean |baseband|² over the wanted channel.
        let power_sum: f32 = baseband.iter().map(|c| c.re * c.re + c.im * c.im).sum();
        self.last_channel_power = power_sum / baseband.len() as f32;

        // Stage 2: one-sided analytic band-pass (USB) at 48 kHz.
        let z = self.analytic.process(&baseband);

        // Real part = wanted-sideband audio; linear level from |z|; digital
        // AGC normalises the output level (meter stays on the pre-AGC |z|).
        if self.audio_scratch.len() < z.len() {
            self.audio_scratch.resize(z.len(), 0.0);
        }
        let audio = &mut self.audio_scratch[..z.len()];
        let mut peak = 0.0f32;
        let mut sumsq = 0.0f32;
        let floor = AGC_TARGET_ENV / AGC_MAX_GAIN; // env below this → gain caps
        for (i, c) in z.iter().enumerate() {
            let m2 = c.re * c.re + c.im * c.im;
            if m2 > peak {
                peak = m2;
            }
            sumsq += m2;
            // Fast-attack / slow-release envelope follower on |z|.
            let env = m2.sqrt();
            let coef = if env > self.agc_env { self.agc_attack } else { self.agc_release };
            self.agc_env += coef * (env - self.agc_env);
            let gain = if self.agc {
                (AGC_TARGET_ENV / self.agc_env.max(floor)).min(AGC_MAX_GAIN)
            } else {
                1.0
            };
            // Slow gain commutes with the (short-memory) HPF below.
            audio[i] = gain * c.re;
        }
        self.last_level_peak = peak.sqrt();
        self.last_level_rms = (sumsq / z.len() as f32).sqrt();

        // Stage 3: sub-audio HPF (carrier/DC reject). No de-emphasis.
        self.hpf.process(audio);

        audio.to_vec()
    }

    /// Reset every internal state — between unrelated sessions / retunes.
    pub fn reset(&mut self) {
        self.xlating.reset();
        self.analytic.reset();
        self.hpf.reset();
        self.audio_scratch.clear();
        self.last_channel_power = 0.0;
        self.last_level_peak = 0.0;
        self.last_level_rms = 0.0;
        self.agc_env = AGC_TARGET_ENV;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Synthesize an SDR IQ stream carrying a single USB tone: a real
    /// audio tone at `f_audio`, transmitted as USB with the suppressed
    /// carrier parked at `f_sc` within the captured band.
    fn synth_usb_iq(f_audio: f32, f_sc: f32, input_rate: f32, n: usize) -> Vec<Complex32> {
        // USB of a single tone = a single complex exponential at
        // f_sc + f_audio (the analytic upconversion of cos is exp(+j)).
        (0..n)
            .map(|k| {
                let phi = 2.0 * PI * (f_sc + f_audio) * k as f32 / input_rate;
                Complex32::new(phi.cos(), phi.sin())
            })
            .collect()
    }

    /// Config with the digital AGC disabled — for tests that measure the
    /// raw filter response (AGC would boost a near-silent rejected output).
    fn cfg_no_agc(input_rate_hz: u32, bw: f32, lo_offset: f32) -> SsbRxChainConfig {
        let mut c = SsbRxChainConfig::new(input_rate_hz, bw, lo_offset);
        c.agc = false;
        c
    }

    /// A USB tone at +1000 Hz lands at 1000 Hz in the audio; its LSB
    /// mirror (carrier − 1000) is rejected. AGC off so this measures the
    /// analytic filter, not the level normaliser.
    #[test]
    fn usb_tone_lands_in_audio_and_mirror_is_rejected() {
        let input_rate = 576_000.0_f32;
        let n = 576_000usize; // 1 s
        // Pluto-style: lo_offset 0, carrier at DC of the captured band.
        let mut chain = SsbRxChain::new(cfg_no_agc(576_000, 2700.0, 0.0));
        let usb = synth_usb_iq(1000.0, 0.0, input_rate, n);
        let audio = chain.process(&usb);
        let skip = 4_000usize;
        let rms_usb: f32 = (audio.iter().skip(skip).map(|s| s * s).sum::<f32>()
            / (audio.len() - skip) as f32)
            .sqrt();
        assert!(rms_usb > 0.05, "USB tone should pass, rms={rms_usb}");

        // LSB mirror: tone at carrier − 1000 = −1000 in baseband.
        let mut chain2 = SsbRxChain::new(cfg_no_agc(576_000, 2700.0, 0.0));
        let lsb = synth_usb_iq(-1000.0, 0.0, input_rate, n);
        let audio2 = chain2.process(&lsb);
        let rms_lsb: f32 = (audio2.iter().skip(skip).map(|s| s * s).sum::<f32>()
            / (audio2.len() - skip) as f32)
            .sqrt();
        let rej_db = 20.0 * (rms_lsb / rms_usb).log10();
        assert!(rej_db <= -60.0, "LSB mirror rejection = {rej_db:.1} dB (want <= -60)");
    }

    /// RMS (dBFS) of the settled tail of a 1 kHz USB tone of baseband
    /// amplitude `amp` through the chain, with AGC on/off.
    fn ssb_tone_tail_dbfs(amp: f32, agc: bool) -> f32 {
        let input_rate = 576_000u32;
        let n = 576_000usize * 2; // 2 s — lets the AGC settle
        let mut cfg = SsbRxChainConfig::new(input_rate, 2700.0, 0.0);
        cfg.agc = agc;
        let mut chain = SsbRxChain::new(cfg);
        let iq: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * 1000.0 * k as f32 / input_rate as f32;
                Complex32::new(amp * phi.cos(), amp * phi.sin())
            })
            .collect();
        let audio = chain.process(&iq);
        let tail = &audio[audio.len() * 3 / 4..]; // last 0.5 s, settled
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        20.0 * (rms + 1e-12).log10()
    }

    /// The digital AGC pins a strong in-band signal near the target level
    /// and lifts a weak one well above its un-normalised level — the whole
    /// point of SSB AGC (a linear mode has no built-in level normalisation).
    #[test]
    fn agc_normalises_level() {
        // Strong input (|z|=0.5, −6 dBFS) settles near target (≈ −12 dBFS).
        let strong = ssb_tone_tail_dbfs(0.5, true);
        assert!(strong > -16.0 && strong < -8.0, "strong AGC out = {strong:.1} dBFS (want -16..-8)");

        // A signal within AGC range (|z|=0.03, −30 dBFS) is normalised up to
        // about the same target as the strong one — that's normalisation.
        let mid = ssb_tone_tail_dbfs(0.03, true);
        assert!((mid - strong).abs() < 4.0, "AGC residual spread = {:.1} dB (want < 4)", mid - strong);

        // A very weak signal (|z|=0.004, −48 dBFS) is gain-capped but still
        // lifted by close to the +24 dB max vs AGC-off.
        let weak_on = ssb_tone_tail_dbfs(0.004, true);
        let weak_off = ssb_tone_tail_dbfs(0.004, false);
        let lift = weak_on - weak_off;
        assert!(lift > 18.0, "AGC lift on weak signal = {lift:.1} dB (want > 18)");
    }

    /// The LO-offset path (SDRplay) lands the carrier at DC too: a USB
    /// tone with carrier at −75 kHz and `lo_offset = 75 kHz` recovers the
    /// same audio.
    #[test]
    fn lo_offset_recovers_same_audio() {
        let input_rate = 576_000.0_f32;
        let n = 576_000usize;
        let mut chain = SsbRxChain::new(SsbRxChainConfig::new(576_000, 2700.0, 75_000.0));
        let usb = synth_usb_iq(1000.0, -75_000.0, input_rate, n);
        let audio = chain.process(&usb);
        let skip = 4_000usize;
        let rms: f32 = (audio.iter().skip(skip).map(|s| s * s).sum::<f32>()
            / (audio.len() - skip) as f32)
            .sqrt();
        assert!(rms > 0.05, "LO-offset USB tone should pass, rms={rms}");
    }

    /// Splitting input across two calls is bit-identical to one call.
    #[test]
    fn chunk_boundary_is_seamless() {
        let input_rate = 576_000.0_f32;
        let n = 120_000usize;
        let usb = synth_usb_iq(1200.0, 0.0, input_rate, n);
        let mut a = SsbRxChain::new(SsbRxChainConfig::new(576_000, 2700.0, 0.0));
        let mut b = SsbRxChain::new(SsbRxChainConfig::new(576_000, 2700.0, 0.0));
        let out_a = a.process(&usb);
        let mid = n / 2;
        let mut out_b = b.process(&usb[..mid]);
        out_b.extend(b.process(&usb[mid..]));
        assert_eq!(out_a.len(), out_b.len());
        for (i, (x, y)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert_eq!(x.to_bits(), y.to_bits(), "audio mismatch at {i}: a={x} b={y}");
        }
    }
}
