//! Channel sounder — TX/RX orchestration.
//!
//! Wires the probe generators (`modem-core-base::probe`) and the
//! analysers (`modem-core-base::probe_analyze`) into a workflow that
//! survives the realistic deployment where TX and RX run on two
//! **distinct machines**, each driven by a different operator, with
//! band noise on the capture and no common clock:
//!
//! 1. **TX side** generates a probe sequence that starts with a
//!    [`crate::wake_up_tone`]-equivalent — 1.5 s @ 1500 Hz to engage
//!    the TX rig's VOX, the repeater's squelch, and the RX rig's
//!    squelch — followed by a [`crate::sync_marker`] chirp for
//!    sample-accurate alignment. Then the actual probes interleaved
//!    with silent gaps.
//!
//! 2. **TX side** writes `probe.wav` (what's played) and
//!    `schedule.json` (the sample offsets and probe parameters the
//!    analyser needs). The schedule is small (<1 kB) and lossless;
//!    it's the contract between TX operator and RX operator.
//!
//! 3. **RX side** captures band audio (raw 48 kHz mono WAV) starting
//!    well before the TX engages and stopping well after.
//!
//! 4. **RX side** runs [`analyze_capture`] given the recorded audio
//!    plus the schedule JSON: cross-correlates the recording against
//!    the sync chirp to find the anchor sample, then applies each
//!    probe's analyser to its known offset relative to that anchor.
//!    Writes `signature.json` next to the WAV.
//!
//! Same-machine usage (loopback bring-up, testing) is the trivial case
//! where steps 1-4 run sequentially in one process.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use modem_core_base::probe::{
    awgn, chirp_linear, multitone, silence, sync_marker, tone, two_tone,
    wake_up_tone, LevelStamp,
};
use modem_core_base::probe_analyze::{
    find_sync_marker, measure_chirp, measure_level_sweep, measure_multitone,
    measure_tone, measure_two_tone, ChirpMeasure, LevelSweepMeasure,
    MultiToneMeasure, ToneMeasure, TwoToneMeasure,
};
use modem_core_base::types::AUDIO_RATE;

// --- Channel family ------------------------------------------------------

/// Which radio chain the operator is characterising. Phase 1 only ships
/// `Fm`; `Qo100` and `SsbHf` are reserved for the phase-2 extension
/// (the analyser already returns the relevant per-family parameters
/// as `Option<f32>` so adding a family does not require a schema bump).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelFamily {
    Fm,
    Qo100,
    SsbHf,
}

// --- Probe schedule ------------------------------------------------------

/// Description of a single probe within a sounding schedule. Matches
/// the [`modem_core_base::probe`] generator vocabulary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeSpec {
    /// Single tone — fundamental SNR/phase reference. `amplitude` is
    /// linear in `(0, 1)`.
    Tone { freq_hz: f64, amplitude: f32 },
    /// Two equal-amplitude tones — IMD3 measurement. `amp_each` ≤ 0.5
    /// to keep the sum in [-1, 1].
    TwoTone { f1_hz: f64, f2_hz: f64, amp_each: f32 },
    /// Linear chirp from `f0_hz` to `f1_hz` — group delay + -3 dB BW.
    ChirpLinear { f0_hz: f64, f1_hz: f64, amplitude: f32 },
    /// Multitone — frequency response (gain vs freq).
    Multitone { freqs_hz: Vec<f64>, amp_each: f32 },
    /// AWGN — noise-floor shape, AGC behaviour.
    Awgn { rms: f32, seed: u64 },
    /// Amplitude sweep of an inner probe at the listed dBFS levels —
    /// AM-AM / AM-PM / P1dB / sweet spot. Only `Tone` is supported as
    /// the inner probe in phase 1 (the analyser keys off the tone
    /// freq); pass that freq via `inner_tone_freq_hz`.
    LevelSweep {
        inner_tone_freq_hz: f64,
        levels_db: Vec<f32>,
        duration_s_per_level: f64,
        gap_s: f64,
    },
}

/// Sounding-schedule envelope as it travels across machines.
///
/// All sample indices are at [`AUDIO_RATE`] (48 kHz) and refer to the
/// position **inside `probe.wav`**. The RX analyser cross-correlates
/// the capture against the sync chirp at `sync_marker_start..sync_marker_end`
/// and applies a single offset (`capture_anchor - sync_marker_start`)
/// to map every probe-window into the recording.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeSchedule {
    /// Schema version for forward compatibility. Bump on any
    /// breaking change.
    pub schema_version: u8,
    pub sample_rate: u32,
    /// Wake-up tone segment — engages VOX/squelch chains.
    pub wake_up_start: usize,
    pub wake_up_end: usize,
    /// Sync chirp segment — alignment reference for the analyser.
    pub sync_marker_start: usize,
    pub sync_marker_end: usize,
    /// Probe segments in playback order. Each carries the spec
    /// (for the analyser to pick the right measurer) and the
    /// sample range it occupies inside `probe.wav`.
    pub probes: Vec<ProbeSegment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeSegment {
    pub idx: usize,
    pub spec: ProbeSpec,
    pub start_sample: usize,
    pub end_sample: usize,
    /// For LevelSweep only: nested timestamps of each level segment.
    /// `None` for non-sweep probes.
    #[serde(default)]
    pub level_stamps: Option<Vec<LevelStamp>>,
}

// --- Build side (TX) ----------------------------------------------------

/// Default gap (silence) between consecutive probes — gives the AGC and
/// squelch a quiet edge to settle on.
pub const INTER_PROBE_GAP_S: f64 = 0.3;

/// User-driven sounding request — what the TX operator picks in the
/// GUI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoundingRequest {
    pub channel_family: ChannelFamily,
    pub probes: Vec<ProbeSpec>,
    /// Amplitude of the wake-up tone (1.5 s @ 1500 Hz). 0.6 is the
    /// safe default — comfortably under the soundcard ceiling, loud
    /// enough to defeat noise-blanker squelches.
    pub wake_up_amplitude: f32,
    /// Amplitude of the sync chirp. 0.7 is the recommended default —
    /// the chirp has lower crest factor than a single tone so we can
    /// push it slightly louder.
    pub sync_marker_amplitude: f32,
    /// Default duration applied to probes that don't carry their own
    /// duration parameter (Tone, TwoTone, ChirpLinear, Multitone,
    /// Awgn).
    pub default_probe_duration_s: f64,
    /// Inter-probe gap in seconds. Override only if a specific link
    /// requires longer squelch settle time.
    pub inter_probe_gap_s: f64,
    pub metadata: SoundingMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoundingMetadata {
    /// Operator callsign of the TX side.
    pub tx_callsign: String,
    /// Operator callsign of the RX side (may be the same in loopback).
    pub rx_callsign: String,
    pub equipment: String,
    #[serde(default)]
    pub notes: String,
    pub ts_unix: i64,
}

/// Build the probe audio + the schedule envelope from a user request.
///
/// Layout:
///
/// ```text
/// [silence 0.5 s] [wake-up 1.5 s] [silence 0.3 s] [sync 0.5 s]
/// [gap] [probe 1] [gap] [probe 2] ... [gap]
/// ```
///
/// The leading 0.5 s of silence keeps the analyser's sync correlator
/// from being defeated by an immediate-start glitch on noisy
/// soundcards; the post-wake-up gap lets squelches lock cleanly
/// before the sync chirp arrives.
pub fn build_probe_schedule(req: &SoundingRequest) -> (Vec<f32>, ProbeSchedule) {
    let mut audio: Vec<f32> = Vec::new();
    let gap = silence(req.inter_probe_gap_s);

    // 1. Pre-roll silence.
    audio.extend(silence(0.5));

    // 2. Wake-up tone.
    let wake_up_start = audio.len();
    audio.extend(wake_up_tone(req.wake_up_amplitude));
    let wake_up_end = audio.len();
    audio.extend(&gap);

    // 3. Sync chirp.
    let sync_marker_start = audio.len();
    audio.extend(sync_marker(req.sync_marker_amplitude));
    let sync_marker_end = audio.len();
    audio.extend(&gap);

    // 4. Each probe.
    let dur = req.default_probe_duration_s;
    let mut probes_out: Vec<ProbeSegment> = Vec::with_capacity(req.probes.len());
    for (idx, spec) in req.probes.iter().enumerate() {
        let start = audio.len();
        let mut level_stamps: Option<Vec<LevelStamp>> = None;
        match spec {
            ProbeSpec::Tone { freq_hz, amplitude } => {
                audio.extend(tone(*freq_hz, dur, *amplitude));
            }
            ProbeSpec::TwoTone { f1_hz, f2_hz, amp_each } => {
                audio.extend(two_tone(*f1_hz, *f2_hz, dur, *amp_each));
            }
            ProbeSpec::ChirpLinear { f0_hz, f1_hz, amplitude } => {
                audio.extend(chirp_linear(*f0_hz, *f1_hz, dur, *amplitude));
            }
            ProbeSpec::Multitone { freqs_hz, amp_each } => {
                audio.extend(multitone(freqs_hz, dur, *amp_each));
            }
            ProbeSpec::Awgn { rms, seed } => {
                audio.extend(awgn(dur, *rms, *seed));
            }
            ProbeSpec::LevelSweep {
                inner_tone_freq_hz,
                levels_db,
                duration_s_per_level,
                gap_s,
            } => {
                let probe_dur = *duration_s_per_level;
                let probe_freq = *inner_tone_freq_hz;
                let (seg_audio, mut seg_stamps) =
                    modem_core_base::probe::level_sweep(
                        |amp| tone(probe_freq, probe_dur, amp),
                        levels_db,
                        *gap_s,
                    );
                // Shift each stamp into the absolute frame of `audio`.
                for s in seg_stamps.iter_mut() {
                    s.start_sample += start;
                    s.end_sample += start;
                }
                audio.extend(&seg_audio);
                level_stamps = Some(seg_stamps);
            }
        }
        let end = audio.len();
        audio.extend(&gap);
        probes_out.push(ProbeSegment {
            idx,
            spec: spec.clone(),
            start_sample: start,
            end_sample: end,
            level_stamps,
        });
    }

    let sched = ProbeSchedule {
        schema_version: 1,
        sample_rate: AUDIO_RATE,
        wake_up_start,
        wake_up_end,
        sync_marker_start,
        sync_marker_end,
        probes: probes_out,
    };
    (audio, sched)
}

// --- Analyse side (RX) --------------------------------------------------

/// Per-probe measurement output, tagged by probe spec so the JSON
/// stays self-describing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeMeasurement {
    Tone { idx: usize, freq_hz: f64, result: ToneMeasure },
    TwoTone { idx: usize, f1_hz: f64, f2_hz: f64, result: TwoToneMeasure },
    Chirp { idx: usize, f0_hz: f64, f1_hz: f64, result: ChirpMeasure },
    Multitone { idx: usize, freqs_hz: Vec<f64>, result: MultiToneMeasure },
    Awgn { idx: usize, rms_measured: f32 },
    LevelSweep {
        idx: usize,
        inner_tone_freq_hz: f64,
        result: LevelSweepMeasure,
    },
}

/// Derived channel parameters — directly consumable by
/// `study/nbfm_channel_sim.simulate()` once `load_signature()` is in
/// place on the Python side.
///
/// Phase-2 fields (`lorentzian_corner_hz`, `fading_doppler_spread_hz`)
/// stay `None` for FM captures; the schema is ready for them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DerivedChannelParams {
    /// Mean SNR (dB) across all single-tone probes that ran.
    pub snr_est_db: f32,
    /// Output-referred third-order intercept (dBFS), or NaN if no
    /// two-tone probe in the schedule.
    pub ip3_dbfs: f32,
    /// 1 dB compression point (input dBFS), NaN if no LevelSweep or
    /// the sweep didn't reach compression.
    pub p1db_dbfs: f32,
    /// Recommended TX-level sweet spot (input dBFS), NaN if no
    /// LevelSweep.
    pub sweet_spot_dbfs: f32,
    /// -3 dB bandwidth from the chirp probe, (low, high) in Hz.
    /// (0, 0) if no chirp.
    pub bw_3db_hz: (f32, f32),
    /// Peak |group-delay deviation| across the chirp band (µs).
    pub group_delay_peak_us: f32,
    /// Multitone noise-floor estimate (dBFS), NaN if no multitone.
    pub noise_floor_dbfs: f32,
    // Phase-2 reserved
    #[serde(default)]
    pub lorentzian_corner_hz: Option<f32>,
    #[serde(default)]
    pub fading_doppler_spread_hz: Option<f32>,
}

/// Full channel signature emitted by [`analyze_capture`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelSignature {
    pub schema_version: u8,
    pub channel_family: ChannelFamily,
    pub metadata: SoundingMetadata,
    /// Absolute sample index inside the capture where the sync chirp
    /// was located. Useful for offline reanalysis.
    pub capture_anchor_sample: usize,
    pub measurements: Vec<ProbeMeasurement>,
    pub derived: DerivedChannelParams,
}

/// Errors returned by [`analyze_capture`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error("sync chirp not found in capture (threshold {0}× RMS not met)")]
    SyncNotFound(f32),
    #[error("capture too short — last probe at sample {needed} but capture has {have}")]
    CaptureTruncated { needed: usize, have: usize },
}

/// Run the analyser on `capture_audio` against the TX-side
/// `schedule` + `req` (the request gives `channel_family` and metadata
/// that aren't in the schedule).
///
/// `peak_threshold_factor` is forwarded to [`find_sync_marker`]; a
/// value around 6.0 keeps the false-positive rate negligible.
pub fn analyze_capture(
    capture_audio: &[f32],
    schedule: &ProbeSchedule,
    channel_family: ChannelFamily,
    metadata: SoundingMetadata,
    peak_threshold_factor: f32,
) -> Result<ChannelSignature, AnalysisError> {
    // 1. Locate sync chirp in the capture.
    let sync_template = sync_marker(0.7); // amplitude irrelevant for corr peak position
    let anchor = find_sync_marker(capture_audio, &sync_template, peak_threshold_factor)
        .ok_or(AnalysisError::SyncNotFound(peak_threshold_factor))?;
    // Offset from TX-side coordinates (schedule.*) to RX-side
    // coordinates (capture_audio[*]).
    let shift = anchor as i64 - schedule.sync_marker_start as i64;

    // 2. Apply each probe's measurer at its known offset.
    let sr = schedule.sample_rate;
    let mut measurements: Vec<ProbeMeasurement> =
        Vec::with_capacity(schedule.probes.len());
    let mut snr_samples: Vec<f32> = Vec::new();
    let mut ip3 = f32::NAN;
    let mut p1db = f32::NAN;
    let mut sweet = f32::NAN;
    let mut bw_3db = (0.0_f32, 0.0_f32);
    let mut gd_peak = 0.0_f32;
    let mut noise_floor = f32::NAN;

    for seg in &schedule.probes {
        let cap_start = (seg.start_sample as i64 + shift).max(0) as usize;
        let cap_end = (seg.end_sample as i64 + shift).max(0) as usize;
        if cap_end > capture_audio.len() {
            return Err(AnalysisError::CaptureTruncated {
                needed: cap_end,
                have: capture_audio.len(),
            });
        }
        // Skip the first/last 12 ms (fade ramp) of every probe segment
        // when measuring steady-state quantities — every generator in
        // `modem_core_base::probe` applies a 10 ms head/tail ramp.
        let fade = (0.012 * sr as f64) as usize;
        let inner_start = cap_start + fade;
        let inner_end = cap_end.saturating_sub(fade);
        if inner_end <= inner_start {
            continue;
        }
        let seg_audio = &capture_audio[inner_start..inner_end];

        match &seg.spec {
            ProbeSpec::Tone { freq_hz, .. } => {
                let r = measure_tone(seg_audio, sr, *freq_hz);
                snr_samples.push(r.snr_db);
                measurements.push(ProbeMeasurement::Tone {
                    idx: seg.idx,
                    freq_hz: *freq_hz,
                    result: r,
                });
            }
            ProbeSpec::TwoTone { f1_hz, f2_hz, .. } => {
                let r = measure_two_tone(seg_audio, sr, *f1_hz, *f2_hz);
                ip3 = r.ip3_dbfs;
                measurements.push(ProbeMeasurement::TwoTone {
                    idx: seg.idx,
                    f1_hz: *f1_hz,
                    f2_hz: *f2_hz,
                    result: r,
                });
            }
            ProbeSpec::ChirpLinear { f0_hz, f1_hz, .. } => {
                let r = measure_chirp(seg_audio, sr, *f0_hz, *f1_hz);
                bw_3db = r.bw_3db_hz;
                gd_peak = r
                    .group_delay_per_freq
                    .iter()
                    .map(|(_, v)| v.abs())
                    .fold(0.0_f32, f32::max);
                measurements.push(ProbeMeasurement::Chirp {
                    idx: seg.idx,
                    f0_hz: *f0_hz,
                    f1_hz: *f1_hz,
                    result: r,
                });
            }
            ProbeSpec::Multitone { freqs_hz, .. } => {
                let r = measure_multitone(seg_audio, sr, freqs_hz);
                noise_floor = r.noise_floor_dbfs;
                measurements.push(ProbeMeasurement::Multitone {
                    idx: seg.idx,
                    freqs_hz: freqs_hz.clone(),
                    result: r,
                });
            }
            ProbeSpec::Awgn { .. } => {
                // RMS of the segment — useful as a noise-floor sanity
                // check and to detect AGC pumping (compare in vs out
                // ratio offline).
                let n = seg_audio.len() as f32;
                let mean = seg_audio.iter().sum::<f32>() / n;
                let var = seg_audio
                    .iter()
                    .map(|&x| (x - mean).powi(2))
                    .sum::<f32>()
                    / n;
                measurements.push(ProbeMeasurement::Awgn {
                    idx: seg.idx,
                    rms_measured: var.sqrt(),
                });
            }
            ProbeSpec::LevelSweep { inner_tone_freq_hz, .. } => {
                // The schedule's level stamps are in TX-side
                // coordinates; shift them into the capture's frame
                // before handing to the analyser. Sort highest-level
                // first so the P1dB walk-down works (the analyser's
                // contract).
                let mut stamps: Vec<LevelStamp> = seg
                    .level_stamps
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|s| LevelStamp {
                        start_sample: (s.start_sample as i64 + shift)
                            .max(0) as usize,
                        end_sample: ((s.end_sample as i64 + shift)
                            .max(0) as usize)
                            .min(capture_audio.len()),
                        level_db: s.level_db,
                    })
                    .collect();
                stamps.sort_by(|a, b| {
                    b.level_db
                        .partial_cmp(&a.level_db)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let r = measure_level_sweep(
                    capture_audio,
                    sr,
                    &stamps,
                    *inner_tone_freq_hz,
                );
                p1db = r.p1db_dbfs;
                sweet = r.sweet_spot_dbfs;
                measurements.push(ProbeMeasurement::LevelSweep {
                    idx: seg.idx,
                    inner_tone_freq_hz: *inner_tone_freq_hz,
                    result: r,
                });
            }
        }
    }

    let snr_est = if snr_samples.is_empty() {
        f32::NAN
    } else {
        snr_samples.iter().sum::<f32>() / snr_samples.len() as f32
    };
    Ok(ChannelSignature {
        schema_version: 1,
        channel_family,
        metadata,
        capture_anchor_sample: anchor,
        measurements,
        derived: DerivedChannelParams {
            snr_est_db: snr_est,
            ip3_dbfs: ip3,
            p1db_dbfs: p1db,
            sweet_spot_dbfs: sweet,
            bw_3db_hz: bw_3db,
            group_delay_peak_us: gd_peak,
            noise_floor_dbfs: noise_floor,
            lorentzian_corner_hz: None,
            fading_doppler_spread_hz: None,
        },
    })
}

// --- File-IO helpers (loaded/saved by the GUI Tauri commands) -----------

/// Output paths for a single sounding session inside
/// `~/.local/share/newmodem/soundings/<id>/`.
pub struct SoundingPaths {
    pub session_dir: PathBuf,
    pub probe_wav: PathBuf,
    pub schedule_json: PathBuf,
    pub capture_wav: PathBuf,
    pub signature_json: PathBuf,
    pub metadata_json: PathBuf,
}

impl SoundingPaths {
    pub fn under(root: &std::path::Path, id: &str) -> Self {
        let dir = root.join(id);
        Self {
            probe_wav: dir.join("probe.wav"),
            schedule_json: dir.join("schedule.json"),
            capture_wav: dir.join("capture.wav"),
            signature_json: dir.join("signature.json"),
            metadata_json: dir.join("metadata.json"),
            session_dir: dir,
        }
    }
}

// --- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_metadata() -> SoundingMetadata {
        SoundingMetadata {
            tx_callsign: "HB9TOB".into(),
            rx_callsign: "HB9TOB".into(),
            equipment: "FTX-1 + soundcard".into(),
            notes: "test".into(),
            ts_unix: 1_715_700_000,
        }
    }

    fn synthetic_request() -> SoundingRequest {
        SoundingRequest {
            channel_family: ChannelFamily::Fm,
            probes: vec![
                ProbeSpec::Tone { freq_hz: 1500.0, amplitude: 0.5 },
                ProbeSpec::ChirpLinear {
                    f0_hz: 300.0,
                    f1_hz: 2500.0,
                    amplitude: 0.5,
                },
                ProbeSpec::LevelSweep {
                    inner_tone_freq_hz: 1500.0,
                    levels_db: vec![0.0, -3.0, -6.0, -9.0, -12.0],
                    duration_s_per_level: 0.3,
                    gap_s: 0.15,
                },
            ],
            wake_up_amplitude: 0.6,
            sync_marker_amplitude: 0.7,
            default_probe_duration_s: 0.6,
            inter_probe_gap_s: 0.2,
            metadata: synthetic_metadata(),
        }
    }

    #[test]
    fn build_schedule_layout_is_consistent() {
        let (audio, sched) = build_probe_schedule(&synthetic_request());
        // The schedule's probe windows must point at non-empty slices
        // inside `audio`, in monotonically increasing order.
        let mut last_end = sched.sync_marker_end;
        for p in &sched.probes {
            assert!(p.start_sample >= last_end);
            assert!(p.end_sample > p.start_sample);
            assert!(p.end_sample <= audio.len());
            last_end = p.end_sample;
        }
        // The wake-up tone segment looks right.
        let wake_dur = (sched.wake_up_end - sched.wake_up_start) as f64
            / sched.sample_rate as f64;
        assert!((wake_dur - 1.5).abs() < 0.01, "wake-up dur {wake_dur}");
        // The sync chirp segment is 0.5 s.
        let sync_dur = (sched.sync_marker_end - sched.sync_marker_start) as f64
            / sched.sample_rate as f64;
        assert!((sync_dur - 0.5).abs() < 0.01, "sync dur {sync_dur}");
    }

    #[test]
    fn analyze_capture_roundtrip_clean_channel() {
        // Build a schedule, prepend band noise + extra silence to
        // simulate the operator hitting record well before TX starts,
        // append band noise to simulate stopping after TX ends.
        let req = synthetic_request();
        let (probe_audio, sched) = build_probe_schedule(&req);
        let mut capture: Vec<f32> = Vec::new();
        capture.extend(modem_core_base::probe::awgn(1.2, 0.02, 7)); // pre-roll band noise
        let tx_start = capture.len();
        capture.extend(&probe_audio);
        capture.extend(modem_core_base::probe::awgn(0.6, 0.02, 9)); // post-roll
        // Add a tiny gaussian noise on top of the probes themselves
        // (clean channel ≠ noise-free channel — real life always has some).
        let probe_range = tx_start..tx_start + probe_audio.len();
        let inj_noise = modem_core_base::probe::awgn(
            probe_audio.len() as f64 / AUDIO_RATE as f64,
            0.005,
            13,
        );
        for (i, &n) in inj_noise.iter().enumerate() {
            capture[probe_range.start + i] += n;
        }
        let sig = analyze_capture(
            &capture,
            &sched,
            ChannelFamily::Fm,
            req.metadata.clone(),
            6.0,
        )
        .expect("analyse_capture should succeed");
        // anchor ≈ tx_start + sync_marker_start (the start of the
        // sync chirp inside probe_audio).
        let expected_anchor = tx_start + sched.sync_marker_start;
        let dev = (sig.capture_anchor_sample as i64 - expected_anchor as i64).abs();
        assert!(
            dev <= 5,
            "anchor {} expected ≈ {}, dev {}",
            sig.capture_anchor_sample,
            expected_anchor,
            dev,
        );
        // 3 probes → 3 measurements.
        assert_eq!(sig.measurements.len(), 3);
        // SNR on the tone probe should be reasonable (signal 0.5,
        // noise rms 0.005 → SNR ≈ 33 dB).
        assert!(sig.derived.snr_est_db > 25.0, "snr {}", sig.derived.snr_est_db);
        // Chirp should give a non-degenerate BW range.
        assert!(sig.derived.bw_3db_hz.1 > sig.derived.bw_3db_hz.0);
        // Level sweep on a linear chain → no compression → p1db NaN
        // but sweet_spot finite (= highest-SNR level).
        assert!(sig.derived.p1db_dbfs.is_nan());
        assert!(sig.derived.sweet_spot_dbfs.is_finite());
    }

    #[test]
    fn analyze_capture_propagates_sync_loss() {
        // Pure noise capture — sync chirp won't match.
        let req = synthetic_request();
        let (_audio, sched) = build_probe_schedule(&req);
        let noise = modem_core_base::probe::awgn(5.0, 0.1, 41);
        let r =
            analyze_capture(&noise, &sched, ChannelFamily::Fm, req.metadata.clone(), 6.0);
        assert!(matches!(r, Err(AnalysisError::SyncNotFound(_))));
    }

    #[test]
    fn analyze_capture_detects_synthetic_compression() {
        // Build a schedule with just a LevelSweep, then synthesize a
        // capture where the high-amplitude segments are compressed.
        // The analyser should recover a finite P1dB.
        let req = SoundingRequest {
            channel_family: ChannelFamily::Fm,
            probes: vec![ProbeSpec::LevelSweep {
                inner_tone_freq_hz: 1500.0,
                levels_db: vec![0.0, -1.0, -2.0, -3.0, -4.0, -6.0, -9.0, -12.0],
                duration_s_per_level: 0.3,
                gap_s: 0.15,
            }],
            wake_up_amplitude: 0.6,
            sync_marker_amplitude: 0.7,
            default_probe_duration_s: 0.5,
            inter_probe_gap_s: 0.2,
            metadata: synthetic_metadata(),
        };
        let (probe_audio, sched) = build_probe_schedule(&req);
        // Loopback through a synthetic compressor: any input above
        // -3 dBFS gets attenuated extra by 0.5·(level + 3) dB.
        // Find the LevelSweep's level stamps to apply this per-segment.
        let sweep_seg = &sched.probes[0];
        let stamps =
            sweep_seg.level_stamps.as_ref().expect("level sweep has stamps");
        let mut capture = vec![0.0_f32; sched.sync_marker_end + probe_audio.len() + 48000];
        // Pre-roll pad: nothing to add (zeros == silence).
        // Place the probe audio at offset 0 of the pre-roll for simplicity.
        capture[..probe_audio.len()].copy_from_slice(&probe_audio);
        // Apply per-segment compression.
        for stamp in stamps {
            let extra_db = if stamp.level_db > -3.0 {
                0.5 * (stamp.level_db - (-3.0))
            } else {
                0.0
            };
            if extra_db <= 0.0 {
                continue;
            }
            let gain = 10.0_f32.powf(-extra_db / 20.0);
            for s in &mut capture[stamp.start_sample..stamp.end_sample] {
                *s *= gain;
            }
        }
        let sig = analyze_capture(
            &capture,
            &sched,
            ChannelFamily::Fm,
            req.metadata.clone(),
            6.0,
        )
        .expect("analyze ok");
        assert!(
            sig.derived.p1db_dbfs.is_finite()
                && (sig.derived.p1db_dbfs - (-1.0)).abs() < 1.5,
            "p1db {}",
            sig.derived.p1db_dbfs,
        );
        // Sweet spot = P1dB − 3 dB ≈ -4 dBFS.
        assert!(
            (sig.derived.sweet_spot_dbfs - (sig.derived.p1db_dbfs - 3.0)).abs()
                < 0.1,
            "sweet {} vs p1db {}",
            sig.derived.sweet_spot_dbfs,
            sig.derived.p1db_dbfs,
        );
    }

    #[test]
    fn schedule_json_roundtrip_preserves_layout() {
        let req = synthetic_request();
        let (_audio, sched) = build_probe_schedule(&req);
        let json = serde_json::to_string(&sched).expect("serialize");
        let back: ProbeSchedule = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.schema_version, sched.schema_version);
        assert_eq!(back.sample_rate, sched.sample_rate);
        assert_eq!(back.probes.len(), sched.probes.len());
        assert_eq!(back.sync_marker_start, sched.sync_marker_start);
    }
}
