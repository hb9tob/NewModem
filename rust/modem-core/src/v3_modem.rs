//! `Modem` trait implementation backed by the existing V3 frame format.
//!
//! Pure wrapper over `profile::ProfileIndex` and friends — no DSP logic
//! lives here. Adding a new V3 profile only requires extending
//! `ProfileIndex::ALL`; the descriptor falls out of `to_config()`.

use crate::profile::{ConstellationType, LdpcRate, ProfileIndex};
use crate::traits::{EncodeRequest, Modem, ModemError, ProfileDescriptor};
use crate::types::{AUDIO_RATE, RRC_SPAN_SYM};
use crate::{frame, modulator, rrc};

const FAMILY: &str = "NBFM-V3";

/// Inter-frame silence between the data superframe and the EOT frame.
/// Hard-coded both with and without the VOX prefix in the legacy CLI.
const INTER_FRAME_SILENCE_S: f64 = 0.2;
/// Silence right after the VOX preamble tone, before the data frame.
const VOX_TAIL_SILENCE_S: f64 = 0.05;
/// VOX preamble carrier amplitude (matches the CLI literal).
const VOX_AMPLITUDE: f32 = 0.5;
/// Trailing silence after the EOT frame when VOX is on (matches the CLI).
const POST_EOT_SILENCE_S: f64 = 0.1;

#[derive(Default, Clone, Copy, Debug)]
pub struct V3Modem;

impl Modem for V3Modem {
    fn family(&self) -> &'static str {
        FAMILY
    }

    fn list_profiles(&self) -> Vec<ProfileDescriptor> {
        // Standards first (in canonical order), then experimentals (also in
        // canonical order). Stable sort preserves the relative order inside
        // each group. The GUI relies on this layout to display experimentals
        // grouped at the end of the combo.
        let mut all: Vec<_> = ProfileIndex::ALL.iter().copied().collect();
        all.sort_by_key(|p| p.is_experimental());
        all.into_iter().map(descriptor_for).collect()
    }

    fn profile_by_name(&self, name: &str) -> Option<ProfileDescriptor> {
        ProfileIndex::ALL
            .iter()
            .copied()
            .find(|p| p.name() == name)
            .map(descriptor_for)
    }

    fn encode_to_samples(&self, req: &EncodeRequest<'_>) -> Result<Vec<f32>, ModemError> {
        // Same call sequence as `modem-cli tx` (cf. modem-cli/src/main.rs
        // Commands::Tx around line 279). Any deviation breaks bit-for-bit
        // compatibility with WAVs produced by the CLI — guarded by the
        // cli_parity integration test in modem-worker.
        let pi = ProfileIndex::ALL
            .iter()
            .copied()
            .find(|p| p.name() == req.profile)
            .ok_or_else(|| ModemError::UnknownProfile(req.profile.to_string()))?;
        let cfg = pi.to_config();

        let symbols = frame::build_superframe_v3_range(
            req.wire_payload,
            &cfg,
            req.session_id,
            req.mime_type,
            req.hash_short,
            req.esi_start,
            req.n_packets,
        );

        let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
            .map_err(|e| {
                ModemError::InvalidRequest(format!(
                    "profile {} has incompatible (Rs={}, tau={}): {e}",
                    req.profile, cfg.symbol_rate, cfg.tau,
                ))
            })?;
        let taps = rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);

        let mut data_modulated =
            modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz);
        let eot_symbols = frame::build_eot_frame(&cfg, req.session_id);
        let mut eot_modulated =
            modulator::modulate(&eot_symbols, sps, pitch, &taps, cfg.center_freq_hz);

        // Layout matches `nbfm-modem tx` (modem-cli/src/main.rs ~line 307):
        // - vox > 0:  tone(vox) + 50ms silence + data + 200ms silence + EOT + 100ms silence
        // - vox == 0: data + 200ms silence + EOT
        let out = if req.vox_seconds > 0.0 {
            let mut out = Vec::new();
            out.extend_from_slice(&modulator::tone(
                cfg.center_freq_hz,
                req.vox_seconds,
                VOX_AMPLITUDE,
            ));
            out.extend_from_slice(&modulator::silence(VOX_TAIL_SILENCE_S));
            out.append(&mut data_modulated);
            out.extend_from_slice(&modulator::silence(INTER_FRAME_SILENCE_S));
            out.append(&mut eot_modulated);
            out.extend_from_slice(&modulator::silence(POST_EOT_SILENCE_S));
            out
        } else {
            data_modulated.extend_from_slice(&modulator::silence(INTER_FRAME_SILENCE_S));
            data_modulated.append(&mut eot_modulated);
            data_modulated
        };
        Ok(out)
    }
}

fn descriptor_for(p: ProfileIndex) -> ProfileDescriptor {
    let cfg = p.to_config();
    let bitrate = cfg.net_bitrate();
    let constellation = match cfg.constellation {
        ConstellationType::Qpsk => "QPSK",
        ConstellationType::Psk8 => "8PSK",
        ConstellationType::Apsk16 => "16-APSK",
        ConstellationType::Apsk32 => "32-APSK",
        ConstellationType::Apsk64 => "64-APSK",
    };
    let ldpc = match cfg.ldpc_rate {
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
        bits_per_symbol: cfg.constellation.bits_per_sym() as u32,
        symbol_rate_bd: cfg.symbol_rate,
        ldpc_rate: cfg.ldpc_rate.rate(),
        experimental: p.is_experimental(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_lists_every_known_profile() {
        let v3 = V3Modem;
        let names: Vec<_> = v3.list_profiles().into_iter().map(|p| p.name).collect();
        for expected in ProfileIndex::ALL {
            assert!(
                names.iter().any(|n| n == expected.name()),
                "missing profile {} in V3Modem::list_profiles",
                expected.name(),
            );
        }
    }

    #[test]
    fn v3_marks_experimentals_correctly() {
        let v3 = V3Modem;
        for p in ProfileIndex::ALL {
            let desc = v3
                .profile_by_name(p.name())
                .unwrap_or_else(|| panic!("missing {}", p.name()));
            assert_eq!(desc.experimental, p.is_experimental(), "{}", p.name());
            assert_eq!(desc.family, FAMILY);
        }
    }

    #[test]
    fn v3_returns_none_for_unknown_profile() {
        assert!(V3Modem.profile_by_name("NOPE").is_none());
    }
}
