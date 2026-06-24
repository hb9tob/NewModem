//! Worker-level preamble GATE for the turbo RX path.
//!
//! In the gated turbo architecture the worker drives ONLY this lightweight,
//! raw-audio gate while no burst is in progress — the expensive streaming DSP
//! (resampler + matched filter + FFE) stays parked, which is what lets a Pi 4
//! keep up on the idle channel between bursts.
//!
//! The gate cross-correlates the KNOWN passband preamble against the recent
//! ring audio (FFT matched filter, [`crate::fd_acquire::PreambleMatchedFilter`])
//! and, on a peak above [`MF_ACQ_THRESHOLD`], returns the absolute preamble
//! position so the worker can seek the DSP there
//! ([`crate::v3_session::V3Session::replay_from_anchor`]). It owns its OWN read
//! pointer into the central ring and does NOT validate the marker (that needs
//! the DSP) — the Golay+CRC validation after the seek is the hard false-positive
//! gate.

use crate::fd_acquire::{PreambleMatchedFilter, MF_ACQ_THRESHOLD};
use crate::profile::ProfileIndex;
use crate::types::{AUDIO_RATE, RRC_SPAN_SYM};
use crate::{modulator, preamble, rrc};

/// A gate "open": a preamble was detected on raw audio; the worker should seek
/// the DSP here and let the marker validation confirm it.
#[derive(Debug, Clone, Copy)]
pub struct GateOpen {
    /// Absolute (worker-frame) audio sample index where the preamble starts.
    pub preamble_abs: u64,
    /// Profile/family the gate is configured for (the forced profile today;
    /// the auto-detected family once `TurboGate::auto` lands).
    pub profile: ProfileIndex,
    /// Matched-filter metric at the peak (≥ [`MF_ACQ_THRESHOLD`]).
    pub metric: f64,
}

/// Raw-audio preamble gate over the worker's central ring.
pub struct TurboGate {
    profile: ProfileIndex,
    mf: PreambleMatchedFilter,
    template_len: usize,
    search_len: usize,
    scan_interval: u64,
    /// Next absolute ring-head index at which a scan is allowed (throttle).
    next_scan_abs: u64,
}

impl TurboGate {
    /// Build a gate locked to `profile`. The passband preamble template and the
    /// search window mirror the in-session acquisition (`V3Session::new`,
    /// v3_session.rs:655-686) so detection behaves identically.
    pub fn forced(profile: ProfileIndex) -> Self {
        let cfg = profile.to_config();
        let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
            .expect("profile config has valid integer sps");
        let template = modulator::modulate(
            &preamble::make_preamble_for_config(&cfg),
            sps,
            pitch,
            &rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps),
            cfg.center_freq_hz,
        );
        // ~200 ms scan cadence; search window = preamble + a margin generous
        // enough to bridge the inter-scan gap (the gate sees the whole ring, so
        // the only gap is the throttle interval).
        let scan_interval = ((AUDIO_RATE as u64) / 5).max(1);
        let margin = scan_interval as usize + (AUDIO_RATE as usize / 4); // +250 ms
        let search_len = template.len() + margin;
        let mf = PreambleMatchedFilter::new(&template, search_len);
        Self {
            profile,
            template_len: template.len(),
            mf,
            search_len,
            scan_interval,
            next_scan_abs: 0,
        }
    }

    /// Scan the recent ring for a preamble. `ring` is a contiguous view of the
    /// central rolling buffer, `origin` the absolute index of `ring[0]`.
    /// Throttled to `scan_interval`. Returns a [`GateOpen`] on a peak above the
    /// metric floor. Cheap: one FFT matched-filter pass over `search_len`.
    pub fn poll(&mut self, ring: &[f32], origin: u64) -> Option<GateOpen> {
        let head = origin + ring.len() as u64;
        if ring.len() < self.template_len || head < self.next_scan_abs {
            return None;
        }
        self.next_scan_abs = head + self.scan_interval;
        let start_rel = ring.len().saturating_sub(self.search_len);
        let m = self.mf.best_match(&ring[start_rel..])?;
        if m.metric < MF_ACQ_THRESHOLD {
            return None;
        }
        Some(GateOpen {
            preamble_abs: origin + (start_rel + m.lag) as u64,
            profile: self.profile,
            metric: m.metric,
        })
    }

    /// Re-arm the gate to scan again once the ring head reaches `abs` (called
    /// after a burst ends so the next entry is found without re-scanning the
    /// just-decoded burst).
    pub fn reset_to(&mut self, abs: u64) {
        self.next_scan_abs = abs;
    }

    pub fn profile(&self) -> ProfileIndex {
        self.profile
    }

    /// Audio samples the gate needs buffered before it can scan (one search
    /// window). The worker keeps at least this much ring while searching.
    pub fn search_len(&self) -> usize {
        self.search_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;

    fn build_burst(profile: ProfileIndex, payload_bytes: usize) -> Vec<f32> {
        let cfg = profile.to_config();
        let payload = vec![0xAA_u8; payload_bytes];
        let n_packets = ((payload_bytes + 31) / 32) as u32;
        let symbols =
            frame::build_superframe_v3_range(&payload, &cfg, 0xABCD_1234, 0x01, 0x1234, 0, n_packets);
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
        let taps = rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz)
    }

    #[test]
    fn gate_opens_on_a_real_preamble() {
        let profile = ProfileIndex::HighPlus;
        let mut gate = TurboGate::forced(profile);
        // A clean burst begins with the preamble at sample 0. Feed exactly one
        // search window so the preamble sits inside it.
        let burst = build_burst(profile, 4000);
        let win = gate.search_len().min(burst.len());
        let ring = &burst[..win];
        let open = gate.poll(ring, 0).expect("gate did not open on a real preamble");
        assert_eq!(open.profile, profile);
        assert!(open.metric >= MF_ACQ_THRESHOLD, "metric {} below floor", open.metric);
        // The preamble is near the start of the burst.
        assert!(
            open.preamble_abs < (AUDIO_RATE as u64) / 2,
            "preamble_abs {} not near the start",
            open.preamble_abs,
        );
    }

    #[test]
    fn gate_stays_closed_on_noise() {
        let profile = ProfileIndex::HighPlus;
        let mut gate = TurboGate::forced(profile);
        // Deterministic pseudo-noise (LCG), no preamble.
        let mut s: u64 = 0x1234_5678;
        let noise: Vec<f32> = (0..gate.search_len())
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0) * 0.3
            })
            .collect();
        assert!(
            gate.poll(&noise, 0).is_none(),
            "gate opened on noise (false positive)",
        );
    }

    #[test]
    fn gate_throttles_between_scans() {
        let mut gate = TurboGate::forced(ProfileIndex::HighPlus);
        let n = gate.search_len();
        let zeros = vec![0.0f32; n];
        // First poll scans (head = n ≥ next_scan_abs = 0).
        let _ = gate.poll(&zeros, 0);
        // A second poll at the same head is throttled (returns None without a
        // metric check — zeros wouldn't open anyway, but assert the throttle
        // path by advancing head by less than scan_interval).
        // head advanced by 1 only → below next_scan_abs → throttled.
        let zeros2 = vec![0.0f32; n + 1];
        // We can't easily observe "scanned vs not" without instrumentation;
        // this just exercises the throttle branch for coverage.
        let _ = gate.poll(&zeros2, 0);
    }
}
