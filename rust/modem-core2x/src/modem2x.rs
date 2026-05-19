//! `Modem` trait implementation backed by the V4 wire format.
//!
//! Mirror of `modem_core::v3_modem::V3Modem` for the 2x family. No DSP
//! logic lives here — pure dispatcher over [`crate::profile2x::ProfileIndex2x`]
//! plus the [`crate::frame2x`] / [`modem_core_base::modulator`] pipeline.
//!
//! Compared to V3:
//!
//! - All 8 profiles are visible to `list_profiles()` and none is flagged
//!   experimental — `HighPlusPlus2x` ships in [`ProfileIndex2x::ALL_AUTO_DETECT`].
//! - The "tx_more" range path uses [`crate::frame2x::build_superframe_v4_range`]
//!   so the worker can emit follow-up bursts at a non-zero `esi_start`,
//!   identical V3 semantics.
//! - There is no separate EOT silence policy — the 2x EOT frame
//!   (`build_eot_frame_v4`) is just one PLHEADER cycle; the same VOX
//!   wrapping convention as V3 is preserved so the rest of the worker
//!   stays format-agnostic.

use modem_core_base::modulator;
use modem_core_base::profile_types::{ConstellationType, LdpcRate};
use modem_core_base::rrc;
use modem_core_base::traits::{EncodeRequest, Modem, ModemError, ProfileDescriptor};
use modem_core_base::types::{AUDIO_RATE, RRC_SPAN_SYM};

use crate::frame2x;
use crate::preburst;
use crate::profile2x::{ModemConfig2x, ProfileIndex2x};

const FAMILY: &str = "NBFM-2x";

/// Inter-frame silence between the data superframe and the EOT frame.
/// Matches the V3 default so the worker silence-detection logic stays
/// the same.
const INTER_FRAME_SILENCE_S: f64 = 0.2;
/// Silence right after the VOX preamble tone, before the data frame.
const VOX_TAIL_SILENCE_S: f64 = 0.05;
/// VOX preamble carrier amplitude (matches V3).
const VOX_AMPLITUDE: f32 = 0.5;
/// Trailing silence after the EOT frame when VOX is on.
const POST_EOT_SILENCE_S: f64 = 0.1;
/// PRBS leadout between the EOT frame and the trailing silence. Drains
/// the RX matched filter (RRC_SPAN_SYM × sps audio samples of memory)
/// AND absorbs the channel sim's tail-clipping artifact at negative
/// drift (`apply_clock_drift` clips `t_src > (n-1)/sr` to the last
/// audio sample → step discontinuity if that boundary lands on the
/// EOT META). PRBS continues the same Lfsr15 stream as the pre-burst
/// and inter-frame fill so a future RX correlator can lock the trailing
/// bits if it ever needs them ([[feedback-tail-pad-useful-content]] —
/// padding is always PRBS, never zeros). 0.1 s @ 48 kHz = 4800 samples
/// covers RRC drain + ±300 ppm × 30 s drift slip + AGC steady-state
/// margin without bloating the wire.
const POST_EOT_LEADOUT_S: f64 = 0.1;

/// Stateless `Modem` implementation for the V4 ("2x") wire format.
#[derive(Default, Clone, Copy, Debug)]
pub struct V4Modem;

impl Modem for V4Modem {
    fn family(&self) -> &'static str {
        FAMILY
    }

    fn list_profiles(&self) -> Vec<ProfileDescriptor> {
        // Canonical ALL order — no experimental partition since 2x ships
        // every profile in the auto-detect set.
        ProfileIndex2x::ALL.iter().copied().map(descriptor_for).collect()
    }

    fn profile_by_name(&self, name: &str) -> Option<ProfileDescriptor> {
        ProfileIndex2x::from_name(name).map(descriptor_for)
    }

    fn encode_to_samples(&self, req: &EncodeRequest<'_>) -> Result<Vec<f32>, ModemError> {
        let pi = ProfileIndex2x::from_name(req.profile)
            .ok_or_else(|| ModemError::UnknownProfile(req.profile.to_string()))?;
        let cfg = pi.to_config();

        let symbols = frame2x::build_superframe_v4_range(
            req.wire_payload,
            &cfg,
            req.session_id,
            req.mime_type,
            req.hash_short,
            req.esi_start,
            req.n_packets,
        );

        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau)
                .map_err(|e| {
                    ModemError::InvalidRequest(format!(
                        "profile {} has incompatible (Rs={}, tau={}): {e}",
                        req.profile, cfg.base.symbol_rate, cfg.base.tau,
                    ))
                })?;
        let taps = rrc::rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);

        let mut data_modulated =
            modulator::modulate(&symbols, sps, pitch, &taps, cfg.base.center_freq_hz);
        let eot_symbols = frame2x::build_eot_frame_v4(&cfg, req.session_id);
        let mut eot_modulated =
            modulator::modulate(&eot_symbols, sps, pitch, &taps, cfg.base.center_freq_hz);

        // Wire layout (`vox > 0` = real OTA TX path):
        //   tone(vox) + VOX-tail silence + PRBS pre-burst + data
        //     + PRBS inter-frame + EOT + post-EOT silence
        //
        // The PRBS pre-burst is `PREBURST_LEN_SYM` LFSR-15 QPSK
        // symbols modulated through the SAME RRC + audio carrier as
        // the data, so the FT-991A → FTX-1 sound-card chain's slow
        // AGC sees the operating RMS for ≥ 2 s before any decoded
        // content starts. It also provides 3000 known symbols for
        // one-shot LS FFE training on RX (cf. `preburst.rs`).
        //
        // The pre-burst and inter-frame PRBS share **one Lfsr15
        // stream** : the inter-frame state picks up exactly where the
        // pre-burst left off (offset `PREBURST_LEN_SYM * 2` bits from
        // `LFSR_SEED`). Lets a future RX correlate either section
        // against the same generator advanced the right amount —
        // no silence on the wire between data and EOT.
        //
        // When `vox == 0` (legacy / loopback tests): no pre-burst, no
        // VOX tone. Inter-frame PRBS still emitted with a fresh
        // Lfsr15 (continuation has no meaning when nothing came
        // before).
        let inter_frame_syms_n =
            (INTER_FRAME_SILENCE_S * cfg.base.symbol_rate) as usize;
        let leadout_syms_n =
            (POST_EOT_LEADOUT_S * cfg.base.symbol_rate) as usize;
        let out = if req.vox_seconds > 0.0 {
            let mut prbs = preburst::Lfsr15::new();
            let preburst_syms = prbs.next_qpsk_symbols(preburst::PREBURST_LEN_SYM);
            let preburst_audio = modulator::modulate(
                &preburst_syms,
                sps,
                pitch,
                &taps,
                cfg.base.center_freq_hz,
            );
            // Inter-frame PRBS continues the same LFSR-15 state.
            let inter_frame_syms = prbs.next_qpsk_symbols(inter_frame_syms_n);
            let inter_frame_audio = modulator::modulate(
                &inter_frame_syms,
                sps,
                pitch,
                &taps,
                cfg.base.center_freq_hz,
            );
            // Post-EOT PRBS leadout (drains RRC + absorbs sim clip).
            let leadout_syms = prbs.next_qpsk_symbols(leadout_syms_n);
            let leadout_audio = modulator::modulate(
                &leadout_syms,
                sps,
                pitch,
                &taps,
                cfg.base.center_freq_hz,
            );
            let mut out = Vec::new();
            out.extend_from_slice(&modulator::tone(
                cfg.base.center_freq_hz,
                req.vox_seconds,
                VOX_AMPLITUDE,
            ));
            out.extend_from_slice(&modulator::silence(VOX_TAIL_SILENCE_S));
            out.extend_from_slice(&preburst_audio);
            out.append(&mut data_modulated);
            out.extend_from_slice(&inter_frame_audio);
            out.append(&mut eot_modulated);
            out.extend_from_slice(&leadout_audio);
            out.extend_from_slice(&modulator::silence(POST_EOT_SILENCE_S));
            out
        } else {
            // No pre-burst : inter-frame PRBS starts a fresh stream
            // from `LFSR_SEED`. Same length as the vox>0 path so the
            // RX silence-gate-free decoder reads consistent data.
            let mut prbs = preburst::Lfsr15::new();
            let inter_frame_syms = prbs.next_qpsk_symbols(inter_frame_syms_n);
            let inter_frame_audio = modulator::modulate(
                &inter_frame_syms,
                sps,
                pitch,
                &taps,
                cfg.base.center_freq_hz,
            );
            data_modulated.extend_from_slice(&inter_frame_audio);
            data_modulated.append(&mut eot_modulated);
            data_modulated
        };
        Ok(out)
    }
}

fn descriptor_for(p: ProfileIndex2x) -> ProfileDescriptor {
    let cfg: ModemConfig2x = p.to_config();
    let bitrate = cfg.net_bitrate();
    let constellation = match cfg.base.constellation {
        ConstellationType::Qpsk => "QPSK",
        ConstellationType::Psk8 => "8PSK",
        ConstellationType::Apsk16 => "16-APSK",
        ConstellationType::Apsk32 => "32-APSK",
        ConstellationType::Apsk64 => "64-APSK",
    };
    let ldpc = match cfg.base.ldpc_rate {
        LdpcRate::R1_2 => "1/2",
        LdpcRate::R2_3 => "2/3",
        LdpcRate::R3_4 => "3/4",
        LdpcRate::R5_6 => "5/6",
    };
    let label = format!(
        "{} — {} {}, {:.0} bps",
        p.name(),
        constellation,
        ldpc,
        bitrate,
    );
    ProfileDescriptor {
        name: p.name().to_string(),
        family: FAMILY.to_string(),
        label,
        bitrate_bps: bitrate,
        bits_per_symbol: cfg.base.constellation.bits_per_sym() as u32,
        symbol_rate_bd: cfg.base.symbol_rate,
        ldpc_rate: cfg.base.ldpc_rate.rate(),
        // 2x has no experimental partition: every profile, including
        // HighPlusPlus2x, is in the auto-detect set.
        experimental: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile2x::profile_high_2x;
    use modem_framing::app_header::mime;

    #[test]
    fn family_is_nbfm_2x() {
        assert_eq!(V4Modem.family(), "NBFM-2x");
    }

    #[test]
    fn list_profiles_includes_all_eight() {
        let descriptors = V4Modem.list_profiles();
        assert_eq!(descriptors.len(), 8);
        for (desc, expected) in descriptors.iter().zip(ProfileIndex2x::ALL.iter()) {
            assert_eq!(desc.name, expected.name());
            assert_eq!(desc.family, FAMILY);
            assert!(!desc.experimental, "{} flagged experimental", desc.name);
        }
    }

    #[test]
    fn profile_by_name_canonical_and_unknown() {
        let v4 = V4Modem;
        for p in ProfileIndex2x::ALL {
            let desc = v4
                .profile_by_name(p.name())
                .unwrap_or_else(|| panic!("missing {}", p.name()));
            assert_eq!(desc.name, p.name());
        }
        assert!(v4.profile_by_name("ULTRA").is_none(), "V3 names rejected");
        assert!(v4.profile_by_name("garbage").is_none());
    }

    #[test]
    fn profile_by_name_alias_high_plus() {
        // Keyboard-friendly alias from profile2x::from_name.
        assert_eq!(
            V4Modem.profile_by_name("HIGHPLUS2X").unwrap().name,
            "HIGH+2X"
        );
    }

    #[test]
    fn encode_to_samples_rejects_unknown_profile() {
        let req = EncodeRequest {
            profile: "NOPE",
            wire_payload: &[],
            session_id: 0,
            mime_type: mime::BINARY,
            hash_short: 0,
            esi_start: 0,
            n_packets: 1,
            vox_seconds: 0.0,
        };
        let err = V4Modem.encode_to_samples(&req).unwrap_err();
        assert!(matches!(err, ModemError::UnknownProfile(_)));
    }

    #[test]
    fn encode_to_samples_high_2x_produces_audio() {
        // Drives the encoder end-to-end and checks the output is the
        // expected length: data superframe + 200 ms silence + EOT.
        let payload = vec![0x42u8; 200];
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
        let audio = V4Modem.encode_to_samples(&req).expect("encode ok");

        // Compute the expected sample count.
        let symbols = frame2x::build_superframe_v4_range(
            &payload,
            &cfg,
            0xCAFE,
            mime::BINARY,
            0,
            0,
            n_packets,
        );
        let (sps, _pitch) = rrc::check_integer_constraints(
            AUDIO_RATE,
            cfg.base.symbol_rate,
            cfg.base.tau,
        )
        .unwrap();
        // Modulator returns symbols.len() * sps + (RRC_SPAN_SYM * sps - 1)
        // tail samples (one matched-filter span on each side, minus one).
        // We verify the audio is non-empty and ≥ symbols * sps.
        let lower_bound = symbols.len() * sps;
        assert!(
            audio.len() >= lower_bound,
            "audio={} should be ≥ symbols·sps={}",
            audio.len(),
            lower_bound
        );
        // Audio sample range stays within the modulator's [-1, 1] envelope.
        let max_abs = audio.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
        assert!(max_abs <= 1.0, "audio peak = {} exceeds 1.0", max_abs);
        assert!(max_abs > 0.05, "audio peak = {} suspiciously low", max_abs);
    }

    #[test]
    fn encode_to_samples_with_vox_prepends_tone() {
        let req = EncodeRequest {
            profile: "ULTRA2X",
            wire_payload: &[0u8; 50],
            session_id: 1,
            mime_type: mime::BINARY,
            hash_short: 0,
            esi_start: 0,
            n_packets: 5,
            vox_seconds: 0.5,
        };
        let audio = V4Modem.encode_to_samples(&req).expect("encode ok");
        // VOX = 0.5 s of tone @ 48 kHz = 24 000 samples.
        let vox_len = (0.5 * AUDIO_RATE as f64) as usize;
        // First samples should be the tone — non-zero (amplitude 0.5).
        assert!(audio[100].abs() > 0.1, "VOX tone region appears silent");
        // The VOX path now also emits the PRBS pre-burst between VOX
        // tail-silence and the data superframe. Compute its exact
        // sample count by reproducing the modulator call.
        let cfg = ProfileIndex2x::from_name("ULTRA2X").unwrap().to_config();
        let (sps, pitch) = rrc::check_integer_constraints(
            AUDIO_RATE,
            cfg.base.symbol_rate,
            cfg.base.tau,
        ).unwrap();
        let taps = rrc::rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        let preburst_audio = modulator::modulate(
            preburst::reference_symbols(),
            sps,
            pitch,
            &taps,
            cfg.base.center_freq_hz,
        );
        let req_no_vox = EncodeRequest {
            vox_seconds: 0.0,
            ..req.clone()
        };
        let audio_no_vox = V4Modem.encode_to_samples(&req_no_vox).expect("encode ok");
        let extra = audio.len() - audio_no_vox.len();
        // The vox>0 path also appends a PRBS leadout of
        // `POST_EOT_LEADOUT_S` worth of QPSK symbols between the EOT
        // and the trailing silence. Reproduce the modulator call to
        // count its audio samples exactly.
        let leadout_syms_n =
            (POST_EOT_LEADOUT_S * cfg.base.symbol_rate) as usize;
        let leadout_syms: Vec<num_complex::Complex<f64>> =
            (0..leadout_syms_n).map(|_| num_complex::Complex::new(1.0, 0.0)).collect();
        let leadout_audio = modulator::modulate(
            &leadout_syms,
            sps,
            pitch,
            &taps,
            cfg.base.center_freq_hz,
        );
        let expected_extra = vox_len
            + (VOX_TAIL_SILENCE_S * AUDIO_RATE as f64) as usize
            + preburst_audio.len()
            + leadout_audio.len()
            + (POST_EOT_SILENCE_S * AUDIO_RATE as f64) as usize;
        assert_eq!(
            extra, expected_extra,
            "VOX extra = {} expected {}", extra, expected_extra
        );
    }

    #[test]
    fn descriptor_label_format_consistent_with_v3() {
        // Same "NAME — CONSTELLATION RATE, BITRATE bps" template the
        // GUI combo expects.
        let desc = V4Modem.profile_by_name("HIGH2X").unwrap();
        assert!(desc.label.starts_with("HIGH2X — 16-APSK 3/4"));
        assert!(desc.label.ends_with("bps"));
    }

    #[test]
    fn inter_frame_region_carries_prbs_not_silence() {
        // Build a HIGH+2X burst with vox > 0 and verify the audio
        // region between the data superframe and the EOT frame is
        // non-silent — it must carry the inter-frame PRBS.
        let payload = vec![0xA5u8; 2_000];
        let cfg = ProfileIndex2x::from_name("HIGH+2X").unwrap().to_config();
        let k_bytes = cfg.base.ldpc_rate.k() / 8;
        let k_src = modem_framing::raptorq_codec::k_from_payload(
            payload.len(), k_bytes,
        ) as u32;
        let n_total = k_src
            + modem_framing::raptorq_codec::n_repair_default(k_src);
        let req = EncodeRequest {
            profile: "HIGH+2X",
            wire_payload: &payload,
            session_id: 0x1234_5678,
            mime_type: mime::BINARY,
            hash_short: 0xABCD,
            esi_start: 0,
            n_packets: n_total,
            vox_seconds: 0.5,
        };
        let audio = V4Modem.encode_to_samples(&req).expect("encode ok");

        // Compute the inter-frame region precisely. The pre-EOT
        // sample range starts at end-of-data-superframe and ends at
        // start-of-EOT-frame. Easiest : count back from the end.
        let (sps, _pitch) = rrc::check_integer_constraints(
            AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau,
        ).unwrap();
        let inter_frame_sym_count =
            (INTER_FRAME_SILENCE_S * cfg.base.symbol_rate) as usize;
        let inter_frame_audio_samples = inter_frame_sym_count * sps;
        let post_eot_samples =
            (POST_EOT_SILENCE_S * AUDIO_RATE as f64) as usize;
        let eot_audio_syms = frame2x::eot_frame_symbols_v4(&cfg);
        let eot_audio_samples = eot_audio_syms * sps;

        // Region : [audio.len() - post_eot - eot_audio - inter_frame ..
        //           audio.len() - post_eot - eot_audio].
        let region_end = audio.len() - post_eot_samples - eot_audio_samples;
        let region_start = region_end - inter_frame_audio_samples;
        let region = &audio[region_start..region_end];
        let mean_e2: f64 = region
            .iter()
            .map(|&s| (s as f64) * (s as f64))
            .sum::<f64>()
            / region.len() as f64;
        assert!(
            mean_e2 > 1e-3,
            "inter-frame region is silent (mean_e2={mean_e2:.6}) — \
             expected PRBS audio",
        );
    }
}
