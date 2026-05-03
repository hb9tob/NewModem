//! `Modem` trait implementation backed by the existing V3 frame format.
//!
//! Pure wrapper over `profile::ProfileIndex` and friends — no DSP logic
//! lives here. Adding a new V3 profile only requires extending
//! `ProfileIndex::ALL`; the descriptor falls out of `to_config()`.

use crate::profile::{ConstellationType, LdpcRate, ProfileIndex};
use crate::traits::{Modem, ProfileDescriptor};

const FAMILY: &str = "NBFM-V3";

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
