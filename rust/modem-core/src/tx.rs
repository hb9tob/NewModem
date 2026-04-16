//! Complete TX pipeline: data → audio samples.
//!
//! Combines frame assembly + modulation into a single API.

use crate::frame;
use crate::modulator;
use crate::profile::ModemConfig;
use crate::rrc::{self, rrc_taps};
use crate::types::{AUDIO_RATE, RRC_SPAN_SYM};

/// Transmit data as passband audio samples.
///
/// Full pipeline: data → LDPC encode → interleave → symbol map → TDM pilots
///              → preamble/header insertion → RRC upsample → passband.
///
/// Returns f32 audio samples at 48 kHz.
pub fn tx(data: &[u8], config: &ModemConfig) -> Vec<f32> {
    let (sps, pitch) = rrc::check_integer_constraints(
        AUDIO_RATE,
        config.symbol_rate,
        config.tau,
    )
    .expect("Invalid symbol_rate/tau for integer constraints");

    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);

    // Build symbol stream (preamble + headers + data with pilots)
    let symbols = frame::build_superframe(data, config);

    // Modulate to passband
    modulator::modulate(&symbols, sps, pitch, &taps, config.center_freq_hz)
}

/// Transmit with a leading VOX tone (for PTT activation via VOX).
pub fn tx_with_vox(data: &[u8], config: &ModemConfig, vox_duration_s: f64) -> Vec<f32> {
    let mut samples = Vec::new();

    // VOX preamble: CW tone at center frequency
    if vox_duration_s > 0.0 {
        samples.extend_from_slice(&modulator::tone(config.center_freq_hz, vox_duration_s, 0.5));
    }

    // Silence gap (50ms for transients to settle)
    samples.extend_from_slice(&modulator::silence(0.05));

    // Main transmission
    samples.extend_from_slice(&tx(data, config));

    // Trailing silence (100ms)
    samples.extend_from_slice(&modulator::silence(0.1));

    samples
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{profile_normal, profile_robust, profile_mega, profile_high, profile_ultra};

    #[test]
    fn tx_normal_profile() {
        let config = profile_normal();
        let data = b"Hello NBFM modem!";
        let samples = tx(data, &config);
        assert!(!samples.is_empty());
        // Verify samples are bounded
        assert!(samples.iter().all(|&s| s.abs() <= 1.0));
    }

    #[test]
    fn tx_all_profiles() {
        let data = vec![0xA5u8; 300];
        for (name, config) in [
            ("MEGA", profile_mega()),
            ("HIGH", profile_high()),
            ("NORMAL", profile_normal()),
            ("ROBUST", profile_robust()),
            ("ULTRA", profile_ultra()),
        ] {
            let samples = tx(&data, &config);
            assert!(!samples.is_empty(), "{name} produced no samples");
            let peak = samples.iter().map(|&s| s.abs()).fold(0.0f32, f32::max);
            assert!(
                peak <= 1.0,
                "{name} peak {peak} exceeds 1.0"
            );
            assert!(
                peak > 0.5,
                "{name} peak {peak} too low (modulator issue?)"
            );
        }
    }

    #[test]
    fn tx_with_vox_adds_preamble() {
        let config = profile_normal();
        let data = b"test";
        let without_vox = tx(data, &config);
        let with_vox = tx_with_vox(data, &config, 0.5);

        // With VOX should be longer (0.5s tone + 0.05s silence + 0.1s trailing)
        let extra_samples = (0.5 + 0.05 + 0.1) * 48000.0;
        assert!(
            with_vox.len() > without_vox.len() + extra_samples as usize - 100,
            "VOX version not long enough"
        );
    }

    #[test]
    fn tx_duration_reasonable() {
        let config = profile_normal();
        // 1000 bytes at NORMAL (8PSK 1500Bd rate 1/2): ~2.1 kbps net
        // 1000 * 8 / 2100 ≈ 3.8 seconds of data
        let data = vec![0u8; 1000];
        let samples = tx(&data, &config);
        let duration_s = samples.len() as f64 / 48000.0;
        assert!(
            duration_s > 2.0 && duration_s < 20.0,
            "Duration {duration_s:.1}s seems unreasonable for 1000 bytes at NORMAL"
        );
    }
}
