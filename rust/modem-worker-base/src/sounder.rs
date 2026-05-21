//! Channel sounder — TX/RX orchestration.
//!
//! Wires the probe generators (`modem-core-base::probe`) and the
//! analysers (`modem-core-base::probe_analyze`) into a workflow that
//! survives the realistic deployment where TX and RX run on two
//! **distinct machines**, each driven by a different operator, with
//! band noise on the capture and no common clock:
//!
//! 1. **TX side** generates a probe sequence that starts with a
//!    [`crate::wake_up_tone`]-equivalent — 5 s @ 1750 Hz (the IARU R1
//!    repeater-opening standard) to engage the TX rig's VOX, fire the
//!    repeater open, and settle the RX rig's squelch — followed by a
//!    [`crate::sync_marker`] chirp for sample-accurate alignment.
//!    Then the actual probes interleaved with silent gaps.
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
    awgn, chirp_linear, golay_pair_audio, multitone, silence, sync_marker, tone,
    two_tone, wake_up_tone, LevelStamp,
};
use modem_core_base::probe_analyze::{
    find_sync_marker, measure_chirp, measure_golay, measure_level_sweep,
    measure_multitone, measure_tone, measure_two_tone, ChirpMeasure,
    GolayMeasure, LevelSweepMeasure, MultiToneMeasure, ToneMeasure,
    TwoToneMeasure,
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
    /// Golay complementary-pair channel-impulse-response sounder.
    /// BPSK-modulated, sequences A then B with `gap_s` of silence
    /// between them and another `gap_s` of trailing silence — the
    /// analyser uses both silent intervals as the IR observation
    /// window. Pick `gap_s` ≥ expected channel delay spread.
    GolayPair {
        length_bits: usize,
        chip_rate_hz: f64,
        carrier_hz: f64,
        amplitude: f32,
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
    /// Intended TX level for this probe instance, in dBFS. Set by the
    /// orchestrator / JS when expanding a probe family across the
    /// 11-level grid; `None` for legacy single-level schedules. The
    /// analyser uses this to group measurements by level and pick the
    /// sweet-spot one for the `derived` headline numbers.
    #[serde(default)]
    pub level_db: Option<f32>,
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
    /// Optional parallel array (same length as `probes`) recording the
    /// intended TX dBFS for each entry. Set when expanding a probe
    /// family across the 11-level grid so the analyser can group by
    /// level. `None` slots (or a fully-empty vec) mean "single-level
    /// probe" — the legacy behaviour.
    #[serde(default)]
    pub probe_levels_db: Vec<Option<f32>>,
    /// Amplitude of the wake-up / repeater-opening tone (5 s @ 1750 Hz).
    /// 0.6 is the safe default — comfortably under the soundcard
    /// ceiling, loud enough to defeat noise-blanker squelches and
    /// trigger repeater open-tone detectors that require ~5-10 % FM
    /// deviation.
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
            ProbeSpec::GolayPair {
                length_bits,
                chip_rate_hz,
                carrier_hz,
                amplitude,
                gap_s,
            } => {
                let (seg_audio, _spc) = golay_pair_audio(
                    *length_bits,
                    *chip_rate_hz,
                    *carrier_hz,
                    *amplitude,
                    *gap_s,
                );
                audio.extend(&seg_audio);
            }
        }
        let end = audio.len();
        audio.extend(&gap);
        let level_db = req
            .probe_levels_db
            .get(idx)
            .copied()
            .flatten();
        probes_out.push(ProbeSegment {
            idx,
            spec: spec.clone(),
            start_sample: start,
            end_sample: end,
            level_stamps,
            level_db,
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
    GolayPair {
        idx: usize,
        length_bits: usize,
        chip_rate_hz: f64,
        carrier_hz: f64,
        gap_s: f64,
        result: GolayMeasure,
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
    /// 50 % cumulative-power delay spread (µs) from the Golay
    /// impulse-response probe, NaN if no Golay probe in the schedule.
    /// A bigger number means more group-delay smear / multipath.
    #[serde(default = "f32_nan")]
    pub delay_spread_50_us: f32,
    /// 90 % cumulative-power delay spread (µs), same definition.
    #[serde(default = "f32_nan")]
    pub delay_spread_90_us: f32,
    /// Strongest echo in dBc relative to the main impulse, NaN if no
    /// Golay probe. Useful to detect repeater-relay echoes,
    /// ground-bounce multipath, or filter ringing.
    #[serde(default = "f32_nan")]
    pub strongest_echo_dbc: f32,
    // Phase-2 reserved
    #[serde(default)]
    pub lorentzian_corner_hz: Option<f32>,
    #[serde(default)]
    pub fading_doppler_spread_hz: Option<f32>,
}

fn f32_nan() -> f32 {
    f32::NAN
}

/// Verdict on whether the operator's TX level matches the sweet spot,
/// and how badly the link metrics degrade at the highest tested level.
/// Populated when the schedule contains a level sweep **plus** at
/// least one multi-level non-sweep probe (the standard ramp-first
/// schedule does both); otherwise all fields are NaN / empty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OverModulationVerdict {
    /// Sweet-spot level identified from the AM-AM curve (dBFS).
    pub sweet_spot_dbfs: f32,
    /// Highest level tested in the schedule (dBFS, typically 0).
    pub max_tested_dbfs: f32,
    /// Over-modulation margin = `max_tested - sweet_spot` (dB). A
    /// positive number means the schedule tested above sweet-spot; if
    /// the operator's modem TX is at `max_tested_dbfs` they are
    /// over-modulating by this many dB.
    pub over_modulation_db: f32,
    /// IMD3 at sweet spot (dBc); the realistic figure of merit for
    /// the chain once gain is correctly set.
    pub imd3_at_sweet_dbc: f32,
    /// IMD3 at the highest level (dBc); shows how bad it gets when
    /// driven into compression.
    pub imd3_at_max_dbc: f32,
    /// Human-readable summary: "OK / surmodulé de X dB → IMD3 …".
    pub message: String,
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
    /// Over-modulation verdict — populated when the schedule has both
    /// a level sweep and at least one multi-level non-sweep family.
    /// Empty `Default::default()` when the legacy single-level
    /// schedule is in use.
    #[serde(default)]
    pub verdict: OverModulationVerdict,
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

    // 2. Apply each probe's measurer at its known offset. We collect
    //    `(level_db, kind_tag, ProbeMeasurement)` side-by-side so
    //    pass 3 can group multi-level families by intended TX level
    //    and pick the sweet-spot measurement for the headline numbers.
    let sr = schedule.sample_rate;
    let mut measurements: Vec<ProbeMeasurement> =
        Vec::with_capacity(schedule.probes.len());
    // Parallel array: `levels[i]` is the level_db (or NaN if unknown)
    // associated with `measurements[i]`. We use NaN as a sentinel
    // rather than Option to keep the second-pass picker terse.
    let mut levels: Vec<f32> = Vec::with_capacity(schedule.probes.len());
    let mut snr_samples: Vec<f32> = Vec::new();
    let mut p1db = f32::NAN;
    let mut sweet = f32::NAN;

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

        let lvl = seg.level_db.unwrap_or(f32::NAN);
        match &seg.spec {
            ProbeSpec::Tone { freq_hz, .. } => {
                let r = measure_tone(seg_audio, sr, *freq_hz);
                snr_samples.push(r.snr_db);
                measurements.push(ProbeMeasurement::Tone {
                    idx: seg.idx,
                    freq_hz: *freq_hz,
                    result: r,
                });
                levels.push(lvl);
            }
            ProbeSpec::TwoTone { f1_hz, f2_hz, .. } => {
                let r = measure_two_tone(seg_audio, sr, *f1_hz, *f2_hz);
                measurements.push(ProbeMeasurement::TwoTone {
                    idx: seg.idx,
                    f1_hz: *f1_hz,
                    f2_hz: *f2_hz,
                    result: r,
                });
                levels.push(lvl);
            }
            ProbeSpec::ChirpLinear { f0_hz, f1_hz, .. } => {
                let r = measure_chirp(seg_audio, sr, *f0_hz, *f1_hz);
                measurements.push(ProbeMeasurement::Chirp {
                    idx: seg.idx,
                    f0_hz: *f0_hz,
                    f1_hz: *f1_hz,
                    result: r,
                });
                levels.push(lvl);
            }
            ProbeSpec::Multitone { freqs_hz, .. } => {
                let r = measure_multitone(seg_audio, sr, freqs_hz);
                measurements.push(ProbeMeasurement::Multitone {
                    idx: seg.idx,
                    freqs_hz: freqs_hz.clone(),
                    result: r,
                });
                levels.push(lvl);
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
                levels.push(lvl);
            }
            ProbeSpec::GolayPair {
                length_bits,
                chip_rate_hz,
                carrier_hz,
                gap_s,
                ..
            } => {
                // Golay has no internal fade ramp (modulate_bpsk emits
                // the chips at full amplitude immediately); pass the
                // un-trimmed segment so we don't slice the BPSK head /
                // tail and starve `measure_golay` of the IR-window
                // samples it needs.
                let raw = &capture_audio[cap_start..cap_end];
                let r = measure_golay(
                    raw,
                    sr,
                    *length_bits,
                    *chip_rate_hz,
                    *carrier_hz,
                    *gap_s,
                );
                measurements.push(ProbeMeasurement::GolayPair {
                    idx: seg.idx,
                    length_bits: *length_bits,
                    chip_rate_hz: *chip_rate_hz,
                    carrier_hz: *carrier_hz,
                    gap_s: *gap_s,
                    result: r,
                });
                levels.push(lvl);
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
                levels.push(f32::NAN); // sweep stores its levels inside
            }
        }
    }

    // Headline SNR: prefer the LevelSweep's per-level SNR sample
    // closest to sweet_spot — that's the realistic figure of merit
    // for the link once TX gain is correctly set. Fall back to the
    // mean of standalone Tone probes (legacy single-level schedules),
    // and ultimately to NaN if nothing usable was measured.
    let snr_est = if sweet.is_finite() {
        measurements
            .iter()
            .find_map(|m| match m {
                ProbeMeasurement::LevelSweep { result, .. } => result
                    .snr_db_per_level
                    .iter()
                    .min_by(|a, b| {
                        (a.0 - sweet)
                            .abs()
                            .partial_cmp(&(b.0 - sweet).abs())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(_, snr)| *snr),
                _ => None,
            })
            .unwrap_or_else(|| {
                if snr_samples.is_empty() {
                    f32::NAN
                } else {
                    snr_samples.iter().sum::<f32>() / snr_samples.len() as f32
                }
            })
    } else if snr_samples.is_empty() {
        f32::NAN
    } else {
        snr_samples.iter().sum::<f32>() / snr_samples.len() as f32
    };

    // 3. Pick the sweet-spot instance of each multi-level family for
    //    the headline `derived` numbers — when the schedule is
    //    ramp-first (the new default), each non-sweep family was
    //    emitted 11 times across -30..0 dBFS and the "real" link
    //    quality is whichever instance landed closest to the
    //    sweet-spot level. Falls back gracefully to first-seen when
    //    no level annotation is available (legacy single-level
    //    schedules).
    let pick = |kind_match: &dyn Fn(&ProbeMeasurement) -> bool, target: f32| {
        if !target.is_finite() {
            return measurements.iter().position(kind_match);
        }
        let mut best: Option<(usize, f32)> = None;
        for (i, m) in measurements.iter().enumerate() {
            if !kind_match(m) {
                continue;
            }
            let lv = levels[i];
            let dist = if lv.is_finite() { (lv - target).abs() } else { 1e9 };
            match best {
                None => best = Some((i, dist)),
                Some((_, d)) if dist < d => best = Some((i, dist)),
                _ => {}
            }
        }
        best.map(|(i, _)| i)
    };
    // Same as `pick` but selects the instance with the HIGHEST level
    // — used to capture the "worst case" metric for the verdict
    // (typically the 0 dBFS instance once over-modulation kicks in).
    let pick_highest = |kind_match: &dyn Fn(&ProbeMeasurement) -> bool| {
        let mut best: Option<(usize, f32)> = None;
        for (i, m) in measurements.iter().enumerate() {
            if !kind_match(m) {
                continue;
            }
            let lv = levels[i];
            if !lv.is_finite() {
                continue;
            }
            match best {
                None => best = Some((i, lv)),
                Some((_, l)) if lv > l => best = Some((i, lv)),
                _ => {}
            }
        }
        best.map(|(i, _)| i)
    };

    let is_two_tone = |m: &ProbeMeasurement| matches!(m, ProbeMeasurement::TwoTone { .. });
    let is_chirp = |m: &ProbeMeasurement| matches!(m, ProbeMeasurement::Chirp { .. });
    let is_multitone = |m: &ProbeMeasurement| matches!(m, ProbeMeasurement::Multitone { .. });
    let is_golay = |m: &ProbeMeasurement| matches!(m, ProbeMeasurement::GolayPair { .. });

    let mut ip3 = f32::NAN;
    let mut imd3_sweet_dbc = f32::NAN;
    if let Some(idx) = pick(&is_two_tone, sweet) {
        if let ProbeMeasurement::TwoTone { result, .. } = &measurements[idx] {
            ip3 = result.ip3_dbfs;
            imd3_sweet_dbc = 0.5 * (result.imd3_low_dbc + result.imd3_high_dbc);
        }
    }
    let mut bw_3db = (0.0_f32, 0.0_f32);
    let mut gd_peak = 0.0_f32;
    if let Some(idx) = pick(&is_chirp, sweet) {
        if let ProbeMeasurement::Chirp { result, .. } = &measurements[idx] {
            bw_3db = result.bw_3db_hz;
            gd_peak = result
                .group_delay_per_freq
                .iter()
                .map(|(_, v)| v.abs())
                .fold(0.0_f32, f32::max);
        }
    }
    let mut noise_floor = f32::NAN;
    if let Some(idx) = pick(&is_multitone, sweet) {
        if let ProbeMeasurement::Multitone { result, .. } = &measurements[idx] {
            noise_floor = result.noise_floor_dbfs;
        }
    }
    // For Golay impulse response we deliberately *don't* pick at
    // sweet-spot: the IR measurement is signal-strength-limited (the
    // matched-filter output has to clear the channel noise floor),
    // not linearity-limited. Pick the instance with the highest
    // recovered |h(t)| peak — that's the level where the IR has the
    // best SNR, which gives the cleanest delay-spread / echo numbers.
    let mut delay_50 = f32::NAN;
    let mut delay_90 = f32::NAN;
    let mut echo_dbc = f32::NAN;
    let golay_strongest = {
        let mut best: Option<(usize, f32)> = None;
        for (i, m) in measurements.iter().enumerate() {
            if let ProbeMeasurement::GolayPair { result, .. } = m {
                let p = result.peak_amplitude;
                if p.is_finite() {
                    match best {
                        None => best = Some((i, p)),
                        Some((_, q)) if p > q => best = Some((i, p)),
                        _ => {}
                    }
                }
            }
        }
        best.map(|(i, _)| i)
    };
    if let Some(idx) = golay_strongest {
        if let ProbeMeasurement::GolayPair { result, .. } = &measurements[idx] {
            delay_50 = result.delay_spread_50_us;
            delay_90 = result.delay_spread_90_us;
            echo_dbc = result.strongest_echo_dbc;
        }
    }

    // 4. Build the over-modulation verdict.
    let verdict = build_verdict(
        sweet,
        &measurements,
        &levels,
        imd3_sweet_dbc,
        &is_two_tone,
        &pick_highest,
    );

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
            delay_spread_50_us: delay_50,
            delay_spread_90_us: delay_90,
            strongest_echo_dbc: echo_dbc,
            lorentzian_corner_hz: None,
            fading_doppler_spread_hz: None,
        },
        verdict,
    })
}

/// Assemble the over-modulation verdict from the per-level
/// measurements. Compares the IMD3 at sweet-spot level vs the IMD3 at
/// the highest level the schedule tested. If the schedule didn't run
/// a multi-level two-tone (legacy or partial schedule), the verdict
/// is `Default::default()` (all NaN, empty message).
fn build_verdict(
    sweet_spot_dbfs: f32,
    measurements: &[ProbeMeasurement],
    levels: &[f32],
    imd3_at_sweet_dbc: f32,
    is_two_tone: &dyn Fn(&ProbeMeasurement) -> bool,
    pick_highest: &dyn Fn(&dyn Fn(&ProbeMeasurement) -> bool) -> Option<usize>,
) -> OverModulationVerdict {
    if !sweet_spot_dbfs.is_finite() || !imd3_at_sweet_dbc.is_finite() {
        return OverModulationVerdict::default();
    }
    let max_idx = match pick_highest(is_two_tone) {
        Some(i) => i,
        None => return OverModulationVerdict::default(),
    };
    let max_level = levels[max_idx];
    let imd3_at_max_dbc = match &measurements[max_idx] {
        ProbeMeasurement::TwoTone { result, .. } => {
            0.5 * (result.imd3_low_dbc + result.imd3_high_dbc)
        }
        _ => return OverModulationVerdict::default(),
    };
    let over_db = max_level - sweet_spot_dbfs;
    let imd3_deg = imd3_at_max_dbc - imd3_at_sweet_dbc; // negative dBc difference = worse
    let message = if over_db <= 2.0 {
        format!(
            "OK — sweet-spot à {sweet_spot_dbfs:.0} dBFS, niveau max testé {max_level:.0} dBFS."
        )
    } else {
        format!(
            "⚠️ Niveau max testé {:.0} dBFS = +{:.1} dB au-dessus du sweet-spot ({:.0} dBFS). \
             IMD3 passe de {:.1} dBc (sweet) à {:.1} dBc (max) — dégradation {:+.1} dB.",
            max_level, over_db, sweet_spot_dbfs,
            imd3_at_sweet_dbc, imd3_at_max_dbc, imd3_deg,
        )
    };
    OverModulationVerdict {
        sweet_spot_dbfs,
        max_tested_dbfs: max_level,
        over_modulation_db: over_db,
        imd3_at_sweet_dbc,
        imd3_at_max_dbc,
        message,
    }
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
            probe_levels_db: vec![],
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
        // The wake-up tone segment looks right (5 s @ 1750 Hz).
        let wake_dur = (sched.wake_up_end - sched.wake_up_start) as f64
            / sched.sample_rate as f64;
        assert!((wake_dur - 5.0).abs() < 0.01, "wake-up dur {wake_dur}");
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
            probe_levels_db: vec![],
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
        // Sweet spot = P1dB − 6 dB (changed from −3 dB in the
        // sounder-collector-upload bundle so the operator stays well
        // below compression on a long burst).
        assert!(
            (sig.derived.sweet_spot_dbfs - (sig.derived.p1db_dbfs - 6.0)).abs()
                < 0.1,
            "sweet {} vs p1db {}",
            sig.derived.sweet_spot_dbfs,
            sig.derived.p1db_dbfs,
        );
    }

    #[test]
    fn analyze_capture_recovers_golay_impulse_response() {
        // Schedule with a GolayPair probe; build the audio; roundtrip it
        // unchanged through a synthetic capture (clean channel). The
        // recovered impulse response should peak strongly and the
        // delay spread should be small.
        let req = SoundingRequest {
            channel_family: ChannelFamily::Fm,
            probes: vec![ProbeSpec::GolayPair {
                length_bits: 64,
                chip_rate_hz: 1200.0,
                carrier_hz: 1500.0,
                amplitude: 0.5,
                gap_s: 0.05,
            }],
            wake_up_amplitude: 0.5,
            sync_marker_amplitude: 0.7,
            default_probe_duration_s: 0.5,
            inter_probe_gap_s: 0.3,
            metadata: synthetic_metadata(),
            probe_levels_db: vec![],
        };
        let (probe_audio, sched) = build_probe_schedule(&req);
        let mut capture: Vec<f32> = Vec::new();
        capture.extend(modem_core_base::probe::awgn(1.0, 0.005, 21));
        capture.extend(&probe_audio);
        capture.extend(modem_core_base::probe::awgn(0.5, 0.005, 23));
        let sig = analyze_capture(
            &capture,
            &sched,
            ChannelFamily::Fm,
            req.metadata.clone(),
            6.0,
        )
        .expect("analyze ok");
        // Find the golay measurement.
        let g = sig
            .measurements
            .iter()
            .find_map(|m| match m {
                ProbeMeasurement::GolayPair { result, .. } => Some(result),
                _ => None,
            })
            .expect("golay measurement present");
        assert!(
            g.peak_amplitude > 0.05,
            "peak {} too small — golay didn't lock",
            g.peak_amplitude,
        );
        assert!(
            g.delay_spread_50_us.is_finite() && g.delay_spread_50_us < 2000.0,
            "delay50 {} too large for clean channel",
            g.delay_spread_50_us,
        );
        // Derived fields propagated.
        assert!(sig.derived.delay_spread_50_us.is_finite());
        assert!(sig.derived.strongest_echo_dbc.is_finite());
    }

    #[test]
    fn verdict_flags_over_modulation_from_multi_level_two_tone() {
        // Build a schedule with a level sweep + two two-tone probes at
        // -15 dBFS (sweet-spot region) and 0 dBFS (clipping region).
        // Synthesise a capture where the 0 dBFS two-tone is heavily
        // distorted (strong IMD3 product) and the -15 dBFS one is
        // clean. The verdict should report over-modulation and a
        // measurable IMD3 degradation.
        let f1 = 1300.0_f64;
        let f2 = 1700.0_f64;
        let req = SoundingRequest {
            channel_family: ChannelFamily::Fm,
            probes: vec![
                ProbeSpec::LevelSweep {
                    inner_tone_freq_hz: 1500.0,
                    levels_db: vec![-30.0, -24.0, -18.0, -12.0, -6.0, 0.0],
                    duration_s_per_level: 0.3,
                    gap_s: 0.15,
                },
                ProbeSpec::TwoTone { f1_hz: f1, f2_hz: f2, amp_each: 0.5 * 0.1778 }, // -15 dBFS
                ProbeSpec::TwoTone { f1_hz: f1, f2_hz: f2, amp_each: 0.5 },        // 0 dBFS
            ],
            wake_up_amplitude: 0.5,
            sync_marker_amplitude: 0.7,
            default_probe_duration_s: 0.4,
            inter_probe_gap_s: 0.15,
            metadata: synthetic_metadata(),
            probe_levels_db: vec![None, Some(-15.0), Some(0.0)],
        };
        let (probe_audio, sched) = build_probe_schedule(&req);
        // Inject IMD3 only on the 0 dBFS two-tone segment.
        let mut capture = probe_audio.clone();
        let hi_seg = &sched.probes[2];
        // Add a strong tone at 2·f1−f2 = 900 Hz inside the segment.
        let bad_seg = hi_seg.end_sample - hi_seg.start_sample;
        let imd_freq = 2.0 * f1 - f2;
        for i in 0..bad_seg {
            let t = i as f64 / AUDIO_RATE as f64;
            let s = 0.15 * (2.0 * std::f64::consts::PI * imd_freq * t).sin() as f32;
            capture[hi_seg.start_sample + i] += s;
        }
        let sig = analyze_capture(
            &capture,
            &sched,
            ChannelFamily::Fm,
            req.metadata.clone(),
            6.0,
        )
        .expect("analyze ok");
        // Verdict is populated.
        assert!(
            sig.verdict.sweet_spot_dbfs.is_finite(),
            "sweet spot not detected",
        );
        assert!(
            sig.verdict.max_tested_dbfs == 0.0,
            "max level should be 0 dBFS, got {}",
            sig.verdict.max_tested_dbfs,
        );
        // Over-modulation should be positive (max above sweet spot).
        assert!(
            sig.verdict.over_modulation_db > 0.0,
            "over_modulation_db {} expected > 0",
            sig.verdict.over_modulation_db,
        );
        // IMD3 at max should be much worse than at sweet (i.e.,
        // imd3_at_max_dbc > imd3_at_sweet_dbc since the injected IMD3
        // raises the dBc value).
        assert!(
            sig.verdict.imd3_at_max_dbc > sig.verdict.imd3_at_sweet_dbc + 5.0,
            "IMD3 at max ({}) should be ≥ 5 dB worse than at sweet ({})",
            sig.verdict.imd3_at_max_dbc,
            sig.verdict.imd3_at_sweet_dbc,
        );
        // Message exists and warns.
        assert!(
            sig.verdict.message.contains("surmodulé")
                || sig.verdict.message.contains("au-dessus"),
            "message {} should warn about over-modulation",
            sig.verdict.message,
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
