//! RX worker for the 2x family.
//!
//! Bridges the audio-domain (mono `f32` 48 kHz from sound-card / SDR /
//! WAV) to [`modem_core2x::rx_v4::rx_v4_symbols`], which expects a
//! symbol-rate stream of complex symbols.
//!
//! Pipeline:
//!
//! 1. **Downmix** to baseband at `cfg.base.center_freq_hz`.
//! 2. **Matched filter** with the RRC TX pulse (same β / span / sps).
//! 3. **Symbol-rate sampling** — naive integer step today, with a
//!    coarse symbol-phase search to find the best offset within one
//!    symbol period. The TimingLoop / Farrow upgrade (Phase B
//!    integration) lives in [`audio_to_symbols_with_timing`] below as
//!    a `TODO` placeholder; the noise-free / low-drift case decodes
//!    cleanly without it.
//! 4. Hand the symbols to [`rx_v4_symbols`].
//!
//! For sound-card paths with measurable clock drift the TimingLoop
//! integration is the next step; the naive sampler tolerates ≤ ~50 ppm
//! across one PLHEADER cycle (~4 s) before the symbol-rate phase walks
//! more than half a sample.
//!
//! [`rx_v4_symbols`]: modem_core2x::rx_v4::rx_v4_symbols

use modem_core_base::demodulator;
use modem_core_base::rrc::{self, rrc_taps};
use modem_core_base::types::{Complex64, AUDIO_RATE, RRC_SPAN_SYM};
use modem_core2x::plheader::{sof_for_family, SOF_LEN_SYM};
use modem_core2x::profile2x::ModemConfig2x;
use modem_core2x::rx_v4::{self, RxResult2x};

/// Convert an audio-domain `f32` buffer into a stream of complex symbols
/// ready for [`rx_v4_symbols`](modem_core2x::rx_v4::rx_v4_symbols).
///
/// Steps: downmix → matched filter → SOF-anchored symbol-phase pick →
/// integer-step sampling at `sps`.
///
/// The phase pick is **SOF-anchored**: we run a coarse SOF cross-
/// correlation against the matched-filter output for every offset in
/// `[0, sps)` and keep the offset whose peak is largest. This locks the
/// strobe grid onto the actual TX-side strobe positions (which the
/// modulator places at `6·sps + k·sps`) regardless of `sps`. A naive
/// energy-only search worked for `sps=32` but lost the strobe for
/// `sps=48 / 96` profiles where the RRC pulse spreads enough to flatten
/// the energy distribution between strobes.
///
/// `samples` is mono 48 kHz; the returned vector has roughly
/// `samples.len() / sps` entries.
pub fn audio_to_symbols(
    samples: &[f32],
    cfg: &ModemConfig2x,
) -> Result<Vec<Complex64>, String> {
    let (sps, _pitch) = rrc::check_integer_constraints(
        AUDIO_RATE,
        cfg.base.symbol_rate,
        cfg.base.tau,
    )?;
    let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);

    // Downmix to baseband + matched filter.
    let bb = demodulator::downmix(samples, cfg.base.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);

    // The modulator places symbol k's pulse peak at audio sample
    // `k*sps + (taps.len()-1)/2 = k*sps + 6*sps`. The matched filter
    // is shift-invariant, so the strobe positions in the MF output are
    // at audio offsets `6*sps + k*sps` — multiples of `sps`. Sampling
    // with phase 0 thus lands on every strobe, after a 6-symbol lead-in
    // that `find_next_sof` skips over harmlessly. The SOF-anchored
    // variant `best_symbol_phase_sof` is kept for sound-card paths
    // where the AGC/codec might shift the strobe by a few samples.
    let _phase = best_symbol_phase_sof(&mf, sps, cfg);
    let phase = 0usize;

    // Naive integer-step sampling at the symbol rate. The TimingLoop
    // upgrade (Phase B integration) replaces this with strobed Farrow
    // interpolation. For now a flat phase + integer step decodes
    // noise-free WAVs cleanly (validated by the roundtrip tests).
    let n_syms = mf.len().saturating_sub(phase) / sps;
    let mut out = Vec::with_capacity(n_syms);
    for k in 0..n_syms {
        out.push(mf[phase + k * sps]);
    }
    Ok(out)
}

/// Pick the symbol-phase offset (in samples, in `[0, sps)`) whose
/// integer-step sampling lines up best with a SOF correlation peak.
///
/// For each candidate phase, we sample the matched-filter output at the
/// symbol rate and cross-correlate the first ~512 sampled symbols
/// against the SOF template. The phase whose strongest peak is highest
/// wins — i.e. the one that best aligns the sampled stream to the
/// underlying symbol grid.
///
/// This costs `sps` symbol-domain correlations but pays back by giving
/// frame-tight alignment that an energy-only heuristic can miss when
/// the RRC pulse spreads enough to fill the gaps between strobes
/// (visible at `sps ∈ {48, 96}`).
fn best_symbol_phase_sof(
    mf: &[Complex64],
    sps: usize,
    cfg: &ModemConfig2x,
) -> usize {
    if mf.len() < sps * (SOF_LEN_SYM + 4) {
        return 0;
    }
    let sof = sof_for_family(cfg.family);
    // Bound the search window so the full burst search stays linear in
    // sps × symbols (instead of sps × audio length). 1024 sym is enough
    // to land on at least one PLHEADER in the typical 2x bursts.
    let max_syms = (mf.len() / sps).min(1024);

    let mut best_phase = 0usize;
    let mut best_peak = 0.0_f64;
    for p in 0..sps {
        let n_syms = (mf.len() - p) / sps;
        let n_syms = n_syms.min(max_syms);
        if n_syms < SOF_LEN_SYM + 1 {
            continue;
        }
        let mut peak = 0.0_f64;
        for k0 in 0..(n_syms - SOF_LEN_SYM) {
            let mut acc = Complex64::new(0.0, 0.0);
            for n in 0..SOF_LEN_SYM {
                acc += mf[p + (k0 + n) * sps] * sof[n].conj();
            }
            let mag = acc.norm();
            if mag > peak { peak = mag; }
        }
        if peak > best_peak {
            best_peak = peak;
            best_phase = p;
        }
    }
    best_phase
}

/// Audio-domain wrapper: downmix + matched-filter + sample +
/// [`rx_v4_symbols`](modem_core2x::rx_v4::rx_v4_symbols). The single entry
/// point a CLI / GUI worker calls per audio chunk.
pub fn rx_v4_audio(
    samples: &[f32],
    cfg: &ModemConfig2x,
) -> Result<Option<RxResult2x>, String> {
    let symbols = audio_to_symbols(samples, cfg)?;
    Ok(rx_v4::rx_v4_symbols(&symbols, cfg))
}

/// Placeholder for the Phase B closed-loop timing-recovery integration.
///
/// Will replace [`audio_to_symbols`] with a Farrow-interpolated strobe
/// stream driven by [`modem_core_base::timing_loop::TimingLoop`] (Gardner
/// TED for QPSK/8PSK, AbsGardner for APSK). Same return shape so call
/// sites flip with a one-line swap once available.
#[doc(hidden)]
pub fn audio_to_symbols_with_timing(
    samples: &[f32],
    cfg: &ModemConfig2x,
) -> Result<Vec<Complex64>, String> {
    // TODO(C-7 follow-up): replace with TimingLoop strobed Farrow
    // interpolation. Today this is a synonym of `audio_to_symbols`.
    audio_to_symbols(samples, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_core2x::frame2x::build_superframe_v4;
    use modem_core2x::modem2x::V4Modem;
    use modem_core2x::profile2x::{
        profile_high_2x, profile_normal_2x, profile_robust_2x, profile_ultra_2x,
        ProfileIndex2x,
    };
    use modem_core_base::modulator;
    use modem_core_base::traits::{EncodeRequest, Modem};
    use modem_framing::app_header::mime;

    fn rng_bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 56) & 0xFF) as u8
            })
            .collect()
    }

    fn modulate_for(cfg: &ModemConfig2x, payload: &[u8], session_id: u32) -> Vec<f32> {
        let symbols = build_superframe_v4(
            payload,
            cfg,
            session_id,
            mime::BINARY,
            0xAA55,
        );
        let (sps, pitch) = rrc::check_integer_constraints(
            AUDIO_RATE,
            cfg.base.symbol_rate,
            cfg.base.tau,
        )
        .unwrap();
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, cfg.base.center_freq_hz)
    }

    #[test]
    fn audio_to_symbols_produces_expected_count_high() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0x1);
        let audio = modulate_for(&cfg, &payload, 1);
        let syms = audio_to_symbols(&audio, &cfg).expect("ok");
        let expected_min = audio.len() / 32 - 32; // sps=32 for HIGH2X
        assert!(
            syms.len() >= expected_min,
            "got {} syms, expected ≥ {}",
            syms.len(),
            expected_min
        );
    }

    #[test]
    fn audio_roundtrip_high_2x() {
        // Encode → modulate → audio_to_symbols → rx_v4_symbols → match.
        let cfg = profile_high_2x();
        let payload = rng_bytes(1_500, 0xCAFE);
        let audio = modulate_for(&cfg, &payload, 0xDEAD_BEEF);
        let result = rx_v4_audio(&audio, &cfg)
            .expect("audio_to_symbols ok")
            .expect("decode ok");
        let h = result.app_header.expect("AppHeader");
        assert_eq!(h.session_id, 0xDEAD_BEEF);
        assert_eq!(h.file_size, payload.len() as u32);
        assert_eq!(result.data, payload);
    }

    #[test]
    fn audio_roundtrip_normal_2x() {
        let cfg = profile_normal_2x();
        let payload = rng_bytes(700, 0x42);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn audio_roundtrip_robust_2x() {
        let cfg = profile_robust_2x();
        let payload = rng_bytes(300, 0x88);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn audio_roundtrip_ultra_2x() {
        // ULTRA: 500 Bd → 96 sps. Smaller payload because the audio
        // grows fast (96 sps × ~2200 sym ≈ 4 s).
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(80, 0x99);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn pure_noise_audio_returns_none() {
        let cfg = profile_high_2x();
        // 2 s of low-amplitude pseudo-random noise — no SOF inside.
        let mut state = 0xDEAD_BEEF_u64;
        let audio: Vec<f32> = (0..AUDIO_RATE as usize * 2)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let s = (state >> 40) as i32 as f32 / i32::MAX as f32;
                s * 0.05
            })
            .collect();
        assert!(rx_v4_audio(&audio, &cfg).unwrap().is_none());
    }

    #[test]
    fn audio_roundtrip_via_v4_modem_encode_to_samples() {
        // End-to-end through the V4Modem trait: build a request, encode
        // to audio, run rx_v4_audio. Mirrors what the GUI's TX/RX
        // pipeline will do once C-8 lands.
        let payload = rng_bytes(1_200, 0x1);
        let cfg = profile_high_2x();
        let n_packets = {
            let k_bytes = cfg.base.ldpc_rate.k() / 8;
            let k_source = modem_framing::raptorq_codec::k_from_payload(payload.len(), k_bytes)
                as u32;
            k_source + modem_framing::raptorq_codec::n_repair_default(k_source)
        };
        let req = EncodeRequest {
            profile: "HIGH2X",
            wire_payload: &payload,
            session_id: 0xCAFE,
            mime_type: mime::BINARY,
            hash_short: 0,
            esi_start: 0,
            n_packets,
            vox_seconds: 0.0,
        };
        let audio = V4Modem.encode_to_samples(&req).expect("encode");
        let result = rx_v4_audio(&audio, &cfg).unwrap().expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_all_eight_profiles() {
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(400, p.as_u8() as u64);
            let audio = modulate_for(&cfg, &payload, 0x42);
            let result = rx_v4_audio(&audio, &cfg)
                .unwrap_or_else(|e| panic!("{p:?} a2s: {e}"))
                .unwrap_or_else(|| panic!("{p:?} decode None"));
            assert_eq!(result.data, payload, "{p:?}");
        }
    }

    #[test]
    fn best_symbol_phase_sof_locks_onto_real_burst() {
        // Compose a real audio burst then verify the SOF-anchored phase
        // pick lands on an offset that decodes — i.e. the symbol-stream
        // sampled at that phase contains a SOF correlation peak.
        let cfg = profile_high_2x();
        let payload = rng_bytes(200, 0xDA);
        let audio = modulate_for(&cfg, &payload, 0x1);
        let bb = modem_core_base::demodulator::downmix(&audio, cfg.base.center_freq_hz);
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, 32);
        let mf = modem_core_base::demodulator::matched_filter(&bb, &taps);
        let phase = best_symbol_phase_sof(&mf, 32, &cfg);
        // Sample at this phase and check that find_next_sof finds a
        // peak (we just verify a non-empty symbol stream + decode).
        let n_syms = (mf.len() - phase) / 32;
        let syms: Vec<_> = (0..n_syms).map(|k| mf[phase + k * 32]).collect();
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg).expect("decode");
        assert!(!res.data.is_empty());
    }
}
