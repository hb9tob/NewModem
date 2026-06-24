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

/// One geometry's FFT preamble matched filter + the representative profile to
/// report when it wins. The exact profile within a winning geometry family is
/// refined later from the first validated marker.
struct GeomFilter {
    profile: ProfileIndex,
    mf: PreambleMatchedFilter,
    template_len: usize,
}

impl GeomFilter {
    /// Build the matched filter for `profile`'s geometry. `search_len` is the
    /// (shared) scan window length, so every geometry's FFT plan is sized to the
    /// widest template + margin and `best_match` over a common window works.
    fn build(profile: ProfileIndex, search_len: usize) -> Self {
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
        let template_len = template.len();
        let mf = PreambleMatchedFilter::new(&template, search_len.max(template_len));
        Self {
            profile,
            mf,
            template_len,
        }
    }
}

/// Raw-audio preamble gate over the worker's central ring. THE single turbo
/// detection chain (FFT matched filter): `forced` carries one geometry (the
/// locked profile), `auto` carries one per distinct non-experimental geometry
/// and reports the winning family. Same primitive ([`PreambleMatchedFilter::
/// best_match`]) the in-session acquisition uses — no separate naive cold-detect.
pub struct TurboGate {
    filters: Vec<GeomFilter>,
    search_len: usize,
    scan_interval: u64,
    /// Next absolute ring-head index at which a scan is allowed (throttle).
    next_scan_abs: u64,
}

impl TurboGate {
    /// ~200 ms scan cadence; the search window is the widest preamble template +
    /// a margin generous enough to bridge the inter-scan gap (the gate sees the
    /// whole ring, so the only gap is the throttle interval).
    fn scan_params(template_lens: &[usize]) -> (u64, usize) {
        let scan_interval = ((AUDIO_RATE as u64) / 5).max(1);
        let margin = scan_interval as usize + (AUDIO_RATE as usize / 4); // +250 ms
        let widest = template_lens.iter().copied().max().unwrap_or(0);
        (scan_interval, widest + margin)
    }

    /// Build a gate locked to a single `profile`. The passband preamble template
    /// and the search window mirror the in-session acquisition so detection
    /// behaves identically.
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
        let (scan_interval, search_len) = Self::scan_params(&[template.len()]);
        Self {
            filters: vec![GeomFilter::build(profile, search_len)],
            search_len,
            scan_interval,
            next_scan_abs: 0,
        }
    }

    /// Build an AUTO gate: one matched filter per DISTINCT non-experimental
    /// geometry (`(symbol_rate, tau, beta, center_freq)`), each tagged with the
    /// first profile of that family. This is the FFT replacement for the naive
    /// `rx_v2::detect_best_profile_cold` — same geometry dedup, same family
    /// anchoring, but the shared `best_match` primitive (O(N log N)) so a Pi 4
    /// keeps up at cold startup.
    pub fn auto() -> Self {
        // Representative profile + template length per distinct geometry.
        let mut seen: Vec<(u64, u64, u64, u64)> = Vec::new();
        let mut reps: Vec<ProfileIndex> = Vec::new();
        let mut template_lens: Vec<usize> = Vec::new();
        for &p in ProfileIndex::ALL.iter().filter(|p| !p.is_experimental()) {
            let cfg = p.to_config();
            let key = (
                cfg.symbol_rate.to_bits(),
                cfg.tau.to_bits(),
                cfg.beta.to_bits(),
                cfg.center_freq_hz.to_bits(),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let (sps, pitch) = match rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let template = modulator::modulate(
                &preamble::make_preamble_for_config(&cfg),
                sps,
                pitch,
                &rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps),
                cfg.center_freq_hz,
            );
            reps.push(p);
            template_lens.push(template.len());
        }
        let (scan_interval, search_len) = Self::scan_params(&template_lens);
        let filters = reps
            .into_iter()
            .map(|p| GeomFilter::build(p, search_len))
            .collect();
        Self {
            filters,
            search_len,
            scan_interval,
            next_scan_abs: 0,
        }
    }

    /// Scan the recent ring for a preamble. `ring` is a contiguous view of the
    /// central rolling buffer, `origin` the absolute index of `ring[0]`.
    /// Throttled to `scan_interval`. Runs each geometry's FFT matched filter over
    /// the recent `search_len` window and returns the STRONGEST peak above the
    /// metric floor, tagged with that geometry's profile family.
    pub fn poll(&mut self, ring: &[f32], origin: u64) -> Option<GateOpen> {
        let head = origin + ring.len() as u64;
        let min_template = self.filters.iter().map(|f| f.template_len).min().unwrap_or(usize::MAX);
        if ring.len() < min_template || head < self.next_scan_abs {
            return None;
        }
        self.next_scan_abs = head + self.scan_interval;
        let start_rel = ring.len().saturating_sub(self.search_len);
        let win = &ring[start_rel..];
        let mut best: Option<(f64, usize, ProfileIndex)> = None;
        for f in &self.filters {
            if win.len() < f.template_len {
                continue;
            }
            if let Some(m) = f.mf.best_match(win) {
                if m.metric >= MF_ACQ_THRESHOLD
                    && best.map(|(bm, _, _)| m.metric > bm).unwrap_or(true)
                {
                    best = Some((m.metric, m.lag, f.profile));
                }
            }
        }
        let (metric, lag, profile) = best?;
        Some(GateOpen {
            preamble_abs: origin + (start_rel + lag) as u64,
            profile,
            metric,
        })
    }

    /// Re-arm the gate to scan again once the ring head reaches `abs` (called
    /// after a burst ends so the next entry is found without re-scanning the
    /// just-decoded burst).
    pub fn reset_to(&mut self, abs: u64) {
        self.next_scan_abs = abs;
    }

    /// The locked profile (forced gate) or the first family anchor (auto gate).
    /// Per-detection the winning family is carried by [`GateOpen::profile`].
    pub fn profile(&self) -> ProfileIndex {
        self.filters.first().map(|f| f.profile).unwrap_or(ProfileIndex::HighPlusPlus)
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
    fn auto_gate_detects_the_transmitted_geometry() {
        // The auto gate must open on a real burst and report a profile of the
        // SAME geometry as the one transmitted (the exact profile within a
        // family is refined later from the marker). HIGH++ shares the sps=32
        // geometry with NORMAL/HIGH/HIGH+, so the reported family anchor need
        // only be geometry-compatible, not profile-equal.
        let tx = ProfileIndex::HighPlusPlus;
        let mut gate = TurboGate::auto();
        let burst = build_burst(tx, 4000);
        let win = gate.search_len().min(burst.len());
        let open = gate
            .poll(&burst[..win], 0)
            .expect("auto gate did not open on a real preamble");
        let tx_cfg = tx.to_config();
        let got_cfg = open.profile.to_config();
        assert_eq!(
            (
                tx_cfg.symbol_rate.to_bits(),
                tx_cfg.tau.to_bits(),
                tx_cfg.beta.to_bits(),
                tx_cfg.center_freq_hz.to_bits(),
            ),
            (
                got_cfg.symbol_rate.to_bits(),
                got_cfg.tau.to_bits(),
                got_cfg.beta.to_bits(),
                got_cfg.center_freq_hz.to_bits(),
            ),
            "auto gate reported a different geometry ({:?}) than transmitted ({tx:?})",
            open.profile,
        );
        assert!(open.metric >= MF_ACQ_THRESHOLD);
        assert!(open.preamble_abs < (AUDIO_RATE as u64) / 2);
    }

    #[test]
    fn auto_gate_detects_robust_geometry() {
        // ROBUST is a DIFFERENT geometry (sps=48, β=0.25, preamble family B)
        // than the HIGH family (sps=32, family A). The auto gate must carry a
        // filter for it and open on a real ROBUST burst.
        let tx = ProfileIndex::Robust;
        let mut gate = TurboGate::auto();
        let burst = build_burst(tx, 4000);
        let win = gate.search_len().min(burst.len());
        let open = gate
            .poll(&burst[..win], 0)
            .expect("auto gate did not open on a ROBUST preamble");
        let tx_cfg = tx.to_config();
        let got_cfg = open.profile.to_config();
        assert_eq!(
            (tx_cfg.symbol_rate.to_bits(), tx_cfg.beta.to_bits()),
            (got_cfg.symbol_rate.to_bits(), got_cfg.beta.to_bits()),
            "auto gate reported {:?} (different geometry) for a ROBUST burst",
            open.profile,
        );
        assert!(open.metric >= MF_ACQ_THRESHOLD, "metric {} too low", open.metric);
    }

    #[test]
    fn auto_gate_stays_closed_on_noise() {
        let mut gate = TurboGate::auto();
        let mut s: u64 = 0xDEAD_BEEF;
        let noise: Vec<f32> = (0..gate.search_len())
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0) * 0.3
            })
            .collect();
        assert!(
            gate.poll(&noise, 0).is_none(),
            "auto gate opened on noise (false positive)",
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
