//! Canonical NBFM SDR RX chain — single source of truth shared by
//! every backend (Pluto, SDRplay, future RTL-SDR / Lime).
//!
//! Mirrors GNU Radio's idiomatic flowgraph for narrow-band FM
//! reception, where channel selection happens in a
//! `freq_xlating_fir_filter_ccf` placed *upstream* of `nbfm_rx`:
//!
//! ```text
//! I/Q at input_rate_hz
//!   → FreqXlatingFir(decim, kaiser_lpf, lo_offset_hz, input_rate_hz)
//!         ↪ NCO mixes the channel down to DC (no-op when lo_offset = 0)
//!         ↪ sharp LPF rejects adjacent channels ± DC spike
//!         ↪ decimates to AUDIO_RATE (48 kHz)
//!   → QuadratureDemod (at AUDIO_RATE)
//!   → DeemphasisLpf (300 Hz corner — undoes TX preemphasis)
//!   → SubAudioHpf   (300 Hz corner — rejects CTCSS sub-tones)
//!   → 48 kHz f32 mono audio
//! ```
//!
//! Why one module here and not duplicated in each backend's `rx.rs`:
//! the DSP is the part that **must** be identical between backends.
//! Backends differ in transport (sample format, buffer ownership,
//! callback-driven vs poll-driven, rate negotiation) — that stays
//! backend-specific. The DSP is the shared concern, factored here
//! so SDRplay and Pluto literally execute the same code path. Adding
//! a third backend = one new `rx.rs` that calls `NbfmRxChain::new`
//! and feeds it Complex32, no DSP logic to write.
//!
//! ## Channel-filter spec
//!
//! The Kaiser LPF passband stretches a few kHz beyond the FM
//! deviation to leave room for the audio modulating the carrier
//! (Carson's rule: useful BW ≈ 2·(Δf + f_audio_max)). Defaults:
//!
//! ```text
//! passband_end   = max_deviation_hz + AUDIO_MARGIN_HZ  (=3 kHz)
//! stopband_start = passband_end · TRANSITION_RATIO     (=1.5)
//! stopband_db    = 80 dB                                (Kaiser β ≈ 8)
//! ```
//!
//! For ±5 kHz NBFM (25 kHz spacing) → passband 8 kHz, stopband
//! 12 kHz; for ±2.5 kHz narrow-NFM (12.5 kHz spacing) → passband
//! 5.5 kHz, stopband 8.25 kHz. Kaiser tap counts grow with the
//! input rate — at 576 kS/s and 80 dB the tight filter is a few
//! hundred taps; at 2304 kS/s (Pluto's BBPLL fallback) it scales
//! 4× but still well below 1 % of one Pi 5 core.

use num_complex::Complex32;

use crate::audio_filters::{DeemphasisLpf, SubAudioHpf};
use crate::decimator::kaiser_sinc_taps;
use crate::fm_demod::QuadratureDemod;
use crate::freq_xlating::FreqXlatingFir;
use crate::AUDIO_RATE;

/// Default audio margin added on top of `max_deviation_hz` to size the
/// channel filter's passband. Covers the highest audio component of
/// the NBFM-modulating signal (voice / digital modem audio caps out
/// around 3 kHz for both use cases).
pub const AUDIO_MARGIN_HZ: f32 = 3_000.0;

/// Default ratio between stopband start and passband end. 1.5 leaves
/// half the passband as transition band — comfortable for Kaiser to
/// hit 80 dB without ballooning the tap count, and gives a clean
/// adjacent-channel guard at standard channel spacings.
pub const TRANSITION_RATIO: f32 = 1.5;

/// Default Kaiser stop-band rejection in dB. 80 dB covers a neighbour
/// 60 dB stronger than the wanted signal with 20 dB of margin —
/// enough for any realistic crowded 2 m / 70 cm scenario.
pub const DEFAULT_STOPBAND_DB: f32 = 80.0;

/// Configuration for [`NbfmRxChain`]. One source of truth for the
/// channel-select / demod / audio-filter parameters; backends that
/// don't expose a knob simply leave it at the default.
#[derive(Debug, Clone)]
pub struct NbfmRxChainConfig {
    /// Sample rate at which the input I/Q arrives, in Hz. Must be
    /// an integer multiple of [`AUDIO_RATE`]. Typical values are
    /// 576 000 (preferred path on both Pluto and SDRplay) and
    /// 2 304 000 (Pluto BBPLL fallback when 576 kS/s won't lock).
    pub input_rate_hz: u32,
    /// Maximum FM deviation expected on air, in Hz. Drives both the
    /// channel-filter passband (= `max_deviation_hz + AUDIO_MARGIN_HZ`)
    /// and the discriminator gain (`fs / (2π · max_deviation)`).
    /// 5 000 = standard NBFM, 2 500 = narrow NFM.
    pub max_deviation_hz: f32,
    /// LO-offset compensation, in Hz. **Sign convention**: positive
    /// when the hardware LO was programmed *above* the user's target
    /// frequency. With `LO = rf_user + lo_offset_hz`, the wanted signal
    /// appears at `−lo_offset_hz` in the IQ baseband (= below DC by
    /// the offset). The chain's NCO multiplies by
    /// `exp(+j·2π·lo_offset·n/fs)` to bring that signal back up to
    /// DC.
    ///
    /// Set to **0** on backends with hardware DC compensation (Pluto's
    /// AD9363) — the NCO becomes a no-op. Set to a few tens of kHz on
    /// zero-IF SDRs without a built-in DC blocker (SDRplay) so the
    /// carrier doesn't sit on the LO leakage spike. SDRplay's
    /// `DEFAULT_LO_OFFSET_HZ = +75_000` (Hz) is the typical value.
    pub lo_offset_hz: f32,
    /// Target stop-band rejection of the channel filter, in dB. Drives
    /// the Kaiser β and the FIR tap count. [`DEFAULT_STOPBAND_DB`] is
    /// usually right; lower it only if you measure the CPU and find
    /// it really matters.
    pub stopband_db: f32,
}

impl NbfmRxChainConfig {
    /// Build a config with the defaults filled in — the caller still
    /// has to pick `input_rate_hz`, `max_deviation_hz` and
    /// `lo_offset_hz` (those depend on the backend and on the
    /// operator's preferences).
    pub fn new(input_rate_hz: u32, max_deviation_hz: f32, lo_offset_hz: f32) -> Self {
        Self {
            input_rate_hz,
            max_deviation_hz,
            lo_offset_hz,
            stopband_db: DEFAULT_STOPBAND_DB,
        }
    }
}

/// Stateful NBFM RX chain. Construct once at session open with
/// [`NbfmRxChain::new`]; feed it Complex32 chunks via [`Self::process`]
/// to get 48 kHz mono f32 audio.
///
/// Internal state crosses chunk boundaries (FIR history, NCO phase,
/// discriminator history, IIR audio-filter states), so calling
/// `process(a)` then `process(b)` is bit-identical to
/// `process([a, b].concat())`.
pub struct NbfmRxChain {
    /// Channel-select stage: NCO + sharp LPF + decim from
    /// `input_rate_hz` down to `AUDIO_RATE`. Owns most of the CPU
    /// budget; the rest of the chain runs at audio rate.
    xlating: FreqXlatingFir,
    /// FM discriminator at audio rate. Insensitive to a constant
    /// frequency offset (which would just appear as a fixed audio
    /// DC bias, killed by the HPF) — the NCO inside `xlating` puts
    /// the signal at DC for the cleanest demod.
    demod: QuadratureDemod,
    /// 6 dB/oct shelf filter that undoes the TX preemphasis applied
    /// by every NBFM transmitter (75 µs convention; corner at
    /// 1/(2π·τ) ≈ 2.1 kHz, but here we use the 300 Hz "DC slope"
    /// convention from the audio-filters module which already
    /// matches what's been calibrated against the hardware).
    deemph: DeemphasisLpf,
    /// CTCSS reject. Also kills any residual audio DC the
    /// discriminator emits when the LO offset isn't perfectly tuned
    /// out (constant phase rotation → constant audio sample → DC).
    hpf: SubAudioHpf,
    /// Scratch buffer for the per-chunk demod stage so the audio
    /// path doesn't have to allocate per call.
    audio_scratch: Vec<f32>,
}

impl NbfmRxChain {
    /// Build the chain from a config. Panics if `input_rate_hz` isn't
    /// a positive integer multiple of [`AUDIO_RATE`].
    pub fn new(cfg: NbfmRxChainConfig) -> Self {
        assert!(
            cfg.input_rate_hz > 0 && cfg.input_rate_hz % AUDIO_RATE == 0,
            "input_rate_hz {} must be a positive integer multiple of AUDIO_RATE {}",
            cfg.input_rate_hz,
            AUDIO_RATE
        );
        assert!(
            cfg.max_deviation_hz > 0.0,
            "max_deviation_hz must be positive"
        );
        assert!(
            cfg.stopband_db > 0.0,
            "stopband_db must be positive"
        );
        let decim = (cfg.input_rate_hz / AUDIO_RATE) as usize;

        // Channel filter: Kaiser LPF sized off max_deviation_hz so
        // narrow NFM (±2.5 kHz dev) gets a tighter filter than wide
        // NBFM (±5 kHz dev). The audio margin keeps the audio band
        // (DC up to ~3 kHz of the modulating signal) inside the
        // passband.
        let passband_end = cfg.max_deviation_hz + AUDIO_MARGIN_HZ;
        let stopband_start = passband_end * TRANSITION_RATIO;
        let taps = kaiser_sinc_taps(
            cfg.input_rate_hz as f32,
            passband_end,
            stopband_start,
            cfg.stopband_db,
        );
        // Sign flip: `lo_offset_hz` is the operator's convention
        // ("I offset my LO upward by N Hz"), so the wanted signal sits
        // at `−lo_offset_hz` in the IQ baseband. FreqXlatingFir's
        // `center_freq` is "the input frequency that gets translated
        // down to DC", so we pass the negation. With Pluto's
        // `lo_offset = 0` this still resolves to a no-op NCO.
        let xlating = FreqXlatingFir::new(
            decim,
            taps,
            -cfg.lo_offset_hz,
            cfg.input_rate_hz as f32,
        );

        let demod = QuadratureDemod::new(AUDIO_RATE as f32, cfg.max_deviation_hz);
        let deemph = DeemphasisLpf::new(AUDIO_RATE as f32, DeemphasisLpf::DEFAULT_CORNER_HZ);
        let hpf = SubAudioHpf::new(AUDIO_RATE as f32, SubAudioHpf::DEFAULT_CORNER_HZ);

        Self {
            xlating,
            demod,
            deemph,
            hpf,
            audio_scratch: Vec::new(),
        }
    }

    /// Number of FIR taps in the channel-select filter. Surface used
    /// by tests and by backends that want to log the chain dimensions
    /// at session start.
    #[inline]
    pub fn channel_filter_taps(&self) -> usize {
        self.xlating.num_taps()
    }

    /// Decimation factor of the channel-select stage
    /// (= `input_rate_hz / AUDIO_RATE`).
    #[inline]
    pub fn decimation_factor(&self) -> usize {
        self.xlating.factor()
    }

    /// Process a chunk of complex baseband samples, returning audio
    /// samples at [`AUDIO_RATE`]. Output length is approximately
    /// `iq.len() / decimation_factor()` modulo per-chunk boundary
    /// rounding.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<f32> {
        // Stage 1: channel select — NCO + LPF + decim to 48 kHz I/Q.
        let baseband = self.xlating.process(iq);
        if baseband.is_empty() {
            return Vec::new();
        }

        // Stage 2: FM discriminator at 48 kHz.
        if self.audio_scratch.len() < baseband.len() {
            self.audio_scratch.resize(baseband.len(), 0.0);
        }
        let audio = &mut self.audio_scratch[..baseband.len()];
        self.demod.process(&baseband, audio);

        // Stage 3+4: deemphasis + sub-audio HPF, in place.
        self.deemph.process(audio);
        self.hpf.process(audio);

        // Caller wants an owned Vec to push through an mpsc — copy
        // out of the scratch (which we keep around so the next call
        // re-uses the same allocation).
        audio.to_vec()
    }

    /// Reset every internal state — FIR history, NCO phase,
    /// discriminator's `prev`, IIR filter states. Use between
    /// unrelated sessions (re-tune, source restart) so the previous
    /// session's tail doesn't bleed into the next.
    pub fn reset(&mut self) {
        self.xlating.reset();
        self.demod.reset();
        self.deemph.reset();
        self.hpf.reset();
        self.audio_scratch.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm_mod::PhaseMod;
    use std::f32::consts::PI;

    /// Generate a synthetic complex baseband with an NBFM tone: 1 kHz
    /// audio, ±5 kHz deviation. Same approach as the existing
    /// pm_fm_deemph_round_trip test in audio_filters.rs but built up
    /// here from scratch so we don't entangle test scopes.
    fn synth_nbfm_iq(
        fs_iq_hz: f32,
        f_audio_hz: f32,
        _max_dev_hz: f32,
        carrier_offset_hz: f32,
        amp_audio: f32,
        n_samples: usize,
    ) -> Vec<Complex32> {
        // The PhaseMod block produces I/Q where the instantaneous
        // frequency follows the audio. Build the audio first, run it
        // through PhaseMod, then add a constant carrier-offset NCO
        // shift to place the signal somewhere other than DC.
        let audio: Vec<f32> = (0..n_samples)
            .map(|k| amp_audio * (2.0 * PI * f_audio_hz * k as f32 / fs_iq_hz).sin())
            .collect();
        let pm = PhaseMod::calibrated();
        let baseband = pm.process_alloc(&audio);
        // Apply a constant frequency shift so the signal lives at
        // `carrier_offset_hz` in the I/Q spectrum (mirrors what an
        // SDR with LO offset would deliver).
        let omega = 2.0 * PI * carrier_offset_hz / fs_iq_hz;
        baseband
            .into_iter()
            .enumerate()
            .map(|(k, c)| {
                let phi = omega * k as f32;
                let rot = Complex32::new(phi.cos(), phi.sin());
                c * rot
            })
            .collect()
    }

    /// End-to-end: NBFM signal as the hardware actually delivers it
    /// when the LO is programmed +75 kHz above the user's target.
    /// The signal then sits at **−75 kHz** in the IQ baseband (LO
    /// above carrier ⇒ negative IF), and the chain — given
    /// `lo_offset_hz = +75_000` — must translate it back up to DC
    /// for the discriminator. Catches the sign-of-NCO mistake that
    /// the previous version of this test missed (placing the synth
    /// signal at +offset instead of −offset).
    #[test]
    fn end_to_end_recovers_audio_with_lo_offset() {
        let fs = 576_000.0_f32;
        let max_dev = 5_000.0_f32;
        let f_audio = 1_000.0_f32;
        let amp = 0.1_f32;
        let offset = 75_000.0_f32;
        let n = 12 * 4_000;
        // Signal is BELOW DC by `offset` Hz (hardware reality: LO
        // programmed above the user's freq pushes the wanted signal
        // into negative IF).
        let iq = synth_nbfm_iq(fs, f_audio, max_dev, -offset, amp, n);

        // The chain receives the offset in the operator convention
        // ("LO went up by +offset"); it negates internally to
        // translate the −offset signal back to DC.
        let mut chain = NbfmRxChain::new(NbfmRxChainConfig::new(
            fs as u32, max_dev, offset,
        ));
        let audio = chain.process(&iq);
        assert!(audio.len() >= n / 12 - 2);

        // Skip the FIR fill + IIR settle transient. With 700+ taps
        // Kaiser at 576 → 48 the FIR fill ends near output 70.
        let steady = &audio[200..];
        let rms: f32 = (steady.iter().map(|v| v * v).sum::<f32>() / steady.len() as f32).sqrt();
        // Expected RMS at 1 kHz (well above HPF/deemph corner at 300 Hz):
        // PM gain × (deemph at 1 kHz) ≈ k_p · 300/1000 ≈ 0.3 of the
        // input audio amplitude ≈ 0.3·0.1/√2 = 0.0212. Allow a wide
        // ±3 dB band to absorb the deemph pole's frequency response.
        let expected_rms = PhaseMod::DEFAULT_K_P * 300.0 / max_dev * amp / 2.0_f32.sqrt();
        let err_db = 20.0 * (rms / expected_rms).log10();
        assert!(
            err_db.abs() < 3.0,
            "recovered RMS {rms} (expected ≈ {expected_rms}, err {err_db:.2} dB)"
        );
    }

    /// Same end-to-end but with `lo_offset = 0` (Pluto convention).
    /// Confirms the chain works in both modes from the same code.
    #[test]
    fn end_to_end_recovers_audio_without_lo_offset() {
        let fs = 576_000.0_f32;
        let max_dev = 5_000.0_f32;
        let f_audio = 1_000.0_f32;
        let amp = 0.1_f32;
        let n = 12 * 4_000;
        // Carrier at DC (no offset on the synthetic source either).
        let iq = synth_nbfm_iq(fs, f_audio, max_dev, 0.0, amp, n);

        let mut chain = NbfmRxChain::new(NbfmRxChainConfig::new(fs as u32, max_dev, 0.0));
        let audio = chain.process(&iq);

        let steady = &audio[200..];
        let rms: f32 = (steady.iter().map(|v| v * v).sum::<f32>() / steady.len() as f32).sqrt();
        let expected_rms = PhaseMod::DEFAULT_K_P * 300.0 / max_dev * amp / 2.0_f32.sqrt();
        let err_db = 20.0 * (rms / expected_rms).log10();
        assert!(
            err_db.abs() < 3.0,
            "recovered RMS {rms} (expected ≈ {expected_rms}, err {err_db:.2} dB)"
        );
    }

    /// Adjacent-channel realism test: a 20 dB hotter neighbour at
    /// +25 kHz must NOT swamp the wanted signal's audio. We compute
    /// the wanted-only audio as a reference, then run wanted +
    /// adjacent through a fresh chain and measure the diff. Models
    /// the actual on-air scenario the user complained about — a
    /// crowded 2 m band where neighbours are routinely the strongest
    /// signals on the SDR's wide bandwidth.
    ///
    /// Why not test "adjacent-only audio is silent": a pure tone in
    /// the channel filter's stopband, even attenuated 80 dB, is
    /// still a tone with a definite instantaneous frequency, and
    /// `atan2` is amplitude-blind. The discriminator therefore
    /// outputs a bias / aliased note regardless of how small the
    /// post-filter magnitude is — the channel filter's effect only
    /// shows up when something else (the wanted carrier, or noise)
    /// dominates the IQ phase. That dominance is what we test for.
    #[test]
    fn adjacent_does_not_swamp_wanted() {
        let fs = 576_000.0_f32;
        let max_dev = 5_000.0_f32;
        let f_audio = 1_000.0_f32;
        let amp = 0.1_f32;
        let n = 12 * 4_000;

        let wanted = synth_nbfm_iq(fs, f_audio, max_dev, 0.0, amp, n);
        // Adjacent 20 dB hotter (10× amplitude), at +25 kHz, modulated
        // by a different audio tone so any leak shows up as a distinct
        // spectral component (won't accidentally constructively add
        // to the wanted at 1 kHz).
        let adjacent = synth_nbfm_iq(fs, 1_500.0, max_dev, 25_000.0, amp * 10.0, n);
        let combined: Vec<Complex32> = wanted
            .iter()
            .zip(adjacent.iter())
            .map(|(w, a)| w + a)
            .collect();

        // Wanted-only reference; same config so the FIR/IIR
        // transients align across the two chains, leaving only the
        // adjacent-induced perturbation in the diff.
        let mut chain_ref = NbfmRxChain::new(NbfmRxChainConfig::new(fs as u32, max_dev, 0.0));
        let audio_wanted = chain_ref.process(&wanted);
        let mut chain_combined = NbfmRxChain::new(NbfmRxChainConfig::new(fs as u32, max_dev, 0.0));
        let audio_combined = chain_combined.process(&combined);

        // Skip past the FIR fill + IIR settle.
        let skip = 300;
        let len = audio_wanted.len().min(audio_combined.len()) - skip;
        let diff_rms: f32 = ((0..len)
            .map(|i| (audio_combined[skip + i] - audio_wanted[skip + i]).powi(2))
            .sum::<f32>()
            / len as f32)
            .sqrt();
        let wanted_rms: f32 = (audio_wanted
            .iter()
            .skip(skip)
            .map(|v| v * v)
            .sum::<f32>()
            / (audio_wanted.len() - skip) as f32)
            .sqrt();
        let snr_db = 20.0 * (wanted_rms / diff_rms).log10();
        // 20 dB SNR with a 20 dB hotter adjacent ⇒ the channel filter
        // is supplying ~40 dB of effective rejection in the audio
        // domain — about what we expect after the demod's atan2 +
        // deemph -6 dB/oct on the ~25 kHz aliased component.
        assert!(
            snr_db > 20.0,
            "audio SNR vs 20 dB-hot adjacent = {snr_db:.1} dB (target > 20)"
        );
    }

    /// Tap count for ±5 kHz NBFM at 576 kHz is "reasonable" — exposed
    /// so a regression that explodes the FIR (e.g. a wrong-direction
    /// transition spec) trips this test instead of bleeding into
    /// runtime CPU.
    #[test]
    fn channel_filter_tap_count_is_within_budget() {
        let chain = NbfmRxChain::new(NbfmRxChainConfig::new(576_000, 5_000.0, 75_000.0));
        let n = chain.channel_filter_taps();
        // Rough bound: Kaiser at 80 dB / Δf=4 kHz / Fs=576 kHz lands
        // around 720 taps; we accept anything inside [400, 1500] —
        // wide enough to absorb formula round-off, narrow enough to
        // catch a misuse like swapping passband / stopband.
        assert!(
            (400..=1500).contains(&n),
            "tap count {n} outside [400, 1500]"
        );
    }

    /// Same chain on the same input, called once vs in two halves —
    /// must produce identical audio. Catches state-leak bugs across
    /// xlating + demod + IIR boundaries.
    #[test]
    fn chunk_boundary_audio_matches() {
        let fs = 576_000.0_f32;
        let max_dev = 5_000.0_f32;
        let f_audio = 1_500.0_f32;
        let amp = 0.05_f32;
        let n = 12 * 1_000;
        // Signal at −75 kHz in IQ (LO-above-target convention).
        let iq = synth_nbfm_iq(fs, f_audio, max_dev, -75_000.0, amp, n);

        let mut a = NbfmRxChain::new(NbfmRxChainConfig::new(fs as u32, max_dev, 75_000.0));
        let mut b = NbfmRxChain::new(NbfmRxChainConfig::new(fs as u32, max_dev, 75_000.0));
        let out_a = a.process(&iq);
        let mid = n / 2;
        let mut out_b = b.process(&iq[..mid]);
        out_b.extend(b.process(&iq[mid..]));
        assert_eq!(out_a.len(), out_b.len());
        for (i, (x, y)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert_eq!(
                x.to_bits(),
                y.to_bits(),
                "audio mismatch at sample {i}: a={x} b={y}"
            );
        }
    }

    /// Pluto fallback rate (2304 kS/s, ÷48). Same chain, just a higher
    /// input rate — confirms the construction works and the tap
    /// budget is still finite.
    #[test]
    fn pluto_fallback_rate_works() {
        let fs = 2_304_000_u32;
        let max_dev = 5_000.0_f32;
        let chain = NbfmRxChain::new(NbfmRxChainConfig::new(fs, max_dev, 0.0));
        // ÷48 to land at 48 kHz.
        assert_eq!(chain.decimation_factor(), 48);
        // Tap count grows ~4× with the input rate; bound generously.
        let n = chain.channel_filter_taps();
        assert!(
            (1500..=6000).contains(&n),
            "fallback tap count {n} outside [1500, 6000]"
        );
    }
}
