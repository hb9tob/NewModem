//! Streaming audio → symbol front-end for the 2x family.
//!
//! Replaces the batch `audio_to_symbols` whole-buffer pipeline with a
//! stateful, chunk-by-chunk variant suitable for live capture. The four
//! pieces wired together are exactly the DVB-S2X reference architecture:
//!
//! ```text
//!   audio chunk (f32, mono, 48 kHz)
//!      │
//!      ▼
//!   NCO downmix  (continuous phase across chunks via absolute sample
//!                 counter — no per-chunk phase reset)
//!      │
//!      ▼
//!   RRC matched filter  (re-applied on a small overlap-save buffer each
//!                        chunk; FIR is mode='same', the front-edge warm-
//!                        up is covered by the previous chunk's tail)
//!      │
//!      ▼
//!   Farrow cubic-Lagrange interpolation at fractional strobe positions
//!      │
//!      ▼
//!   Gardner / AbsGardner TED + PI loop (per emitted strobe → fractional
//!                                       correction added to strobe step)
//!      │
//!      ▼
//!   timing-recovered Vec<Complex64> symbols
//! ```
//!
//! What this replaces: the batch `audio_to_symbols` SOF-anchored phase
//! pick + naive integer-step sampling. The streaming variant tracks
//! drift continuously, so a >50 ppm clock mismatch on sound-card paths
//! no longer accumulates over a single PLHEADER cycle and lossy decodes
//! one CW out of every four.
//!
//! What this does NOT do: the symbol-domain state machine. Once symbols
//! land in the caller's buffer, the caller still drives `rx_v4_symbols`
//! periodically (idempotent on a growing buffer). Closed-loop timing
//! makes the periodic re-decode lossless — every CW is sampled once,
//! correctly, regardless of when `rx_v4_symbols` is called.
//!
//! Memory: the internal audio buffer is bounded at
//! `taps.len() + sps + small margin` (~400 samples for HIGH2X), trimmed
//! after every chunk. No unbounded growth.

use std::f64::consts::PI;

use modem_core_base::demodulator;
use modem_core_base::farrow;
use modem_core_base::rrc::{self, rrc_taps};
use modem_core_base::timing_loop::{TedVariant, TimingLoop};
use modem_core_base::types::{Complex64, AUDIO_RATE, RRC_SPAN_SYM};
use modem_core2x::profile2x::ModemConfig2x;

/// Stateful audio → symbol front-end. Construct once per RX session,
/// feed audio chunks via [`process_chunk`].
pub struct StreamingFrontend {
    /// Frozen profile config (constellation, β, fc, sps).
    cfg: ModemConfig2x,
    /// RRC matched-filter taps (same as TX).
    taps: Vec<f64>,
    /// Half-tap-length, for FIR-warmup accounting.
    taps_half: usize,
    /// Nominal samples per symbol (integer from `rrc::check_integer_constraints`).
    sps: usize,
    /// Carrier centre frequency (Hz). Hot path constant.
    center_freq_hz: f64,
    /// Absolute index of the next NCO sample to consume. Lets the NCO
    /// phase stay continuous when the working audio buffer is trimmed
    /// between chunks.
    nco_base_sample: u64,
    /// Working audio buffer. Holds the trimmed tail of the previous
    /// chunk (FIR warmup + strobe context) plus any new chunk samples
    /// not yet strobed.
    audio_buffer: Vec<f32>,
    /// Fractional position of the NEXT strobe inside `audio_buffer`. In
    /// the same sample-index space the MF output uses (mode='same'
    /// means MF index ↔ audio index 1:1).
    strobe_pos: f64,
    /// Closed-loop timing recovery. Variant picked from the
    /// constellation type (AbsGardner for APSK, Gardner for PSK).
    timing_loop: TimingLoop,
}

impl StreamingFrontend {
    /// Construct a front-end for `cfg`. Aligns the initial strobe
    /// position on `taps.len() / 2`, which is where the TX modulator
    /// places the pulse peak for symbol 0 (mirror of the batch
    /// `audio_to_symbols` integer-step lead-in).
    pub fn new(cfg: ModemConfig2x) -> Self {
        let (sps, _pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau)
                .expect("profile must have integer-constrained (Rs, τ)");
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        let taps_half = taps.len() / 2;
        // Use the classic Gardner TED across every 2x constellation —
        // including the APSK rings where DVB-S2X §9.3.2 nominally
        // recommends AbsGardner. Reason: AbsGardner is `|late|² −
        // |early|²`, which on a multi-ring constellation gives a TED
        // whose sign depends on the DATA (next vs previous symbol's
        // ring), so feeding it raw into the PI loop accumulates a
        // data-dependent bias that walks the strobe off the grid even
        // on a noise-free signal. The proper DVB-S2X cure is to gate
        // the TED on unit-magnitude *pilot* symbols, which requires
        // symbol-domain frame knowledge — a Phase D-1c-ii follow-up.
        // For now Gardner-only tracks 50–100 ppm on every 2x profile
        // without the data-induced drift the streaming tests exposed.
        let ted = TedVariant::Gardner;
        Self {
            cfg: cfg.clone(),
            center_freq_hz: cfg.base.center_freq_hz,
            taps,
            taps_half,
            sps,
            nco_base_sample: 0,
            audio_buffer: Vec::new(),
            // Match the batch `audio_to_symbols` indexing: strobe k
            // lands on MF[k·sps]. Symbol 0 of the burst lives at index
            // 0 (with a few samples of FIR warm-up noise before
            // anything real shows up; the downstream `rx_v4_symbols`
            // SOF search walks past that). The first few strobes fall
            // outside Farrow's 4-tap window — `interp_safe` falls back
            // to nearest-sample lookup there, which is exactly what
            // the batch path was doing implicitly.
            strobe_pos: 0.0,
            timing_loop: TimingLoop::with_defaults(ted),
        }
    }

    /// Nominal samples per symbol for this profile. Useful for tests.
    #[inline]
    pub fn sps(&self) -> usize {
        self.sps
    }

    /// Read-only access to the underlying TimingLoop for diagnostics
    /// (`last_err`, `integ` — the steady-state drift estimate).
    #[inline]
    pub fn timing_loop(&self) -> &TimingLoop {
        &self.timing_loop
    }

    /// Append `chunk` to the working buffer and emit every strobe whose
    /// Farrow context fits inside the (possibly extended) buffer. Each
    /// emitted strobe walks the Gardner TED forward by one symbol; the
    /// loop integ converges to the per-symbol drift.
    ///
    /// The function returns a `Vec<Complex64>` of newly-produced
    /// symbols. Empty when the chunk wasn't long enough to cross
    /// another strobe boundary (rare, given chunks are ≥ ~500 samples
    /// and `sps ≤ 96`).
    pub fn process_chunk(&mut self, chunk: &[f32]) -> Vec<Complex64> {
        if chunk.is_empty() {
            return Vec::new();
        }
        self.audio_buffer.extend_from_slice(chunk);
        // Memory is bounded by the post-strobe trim below — the buffer
        // never holds more than `chunk.len() + taps_half + sps + 4`
        // samples across two calls. No defensive cap is needed and a
        // cap here would discard un-strobed audio.

        // Downmix the WHOLE working buffer to baseband. NCO phase is
        // anchored on `nco_base_sample` so chunks chain without a phase
        // glitch (a per-chunk reset to zero would create a step
        // proportional to `fc · (trim distance)`).
        let bb = downmix_with_offset(
            &self.audio_buffer,
            self.center_freq_hz,
            self.nco_base_sample,
        );
        // RRC matched filter, mode='same'. For our ≤ ~600-sample
        // working buffer this is the direct O(N·M) path (well below
        // demodulator's FFT_THRESHOLD = 2048), so it stays cheap.
        let mf = demodulator::matched_filter(&bb, &self.taps);

        let half_sps = self.sps as f64 * 0.5;
        let mut symbols = Vec::new();
        loop {
            // We need MF samples up to `strobe_pos + sps/2 + 2` for the
            // LATE Farrow window. If the buffer doesn't extend that far
            // yet, stop and wait for the next chunk.
            let late = self.strobe_pos + half_sps;
            if late + 2.0 >= mf.len() as f64 {
                break;
            }
            let early = self.strobe_pos - half_sps;
            // Gardner runs only when ALL three Farrow lookups land
            // inside the cubic-Lagrange's safe zone (`1 ≤ pos ≤ N-3`).
            // The very first strobes of a fresh frontend (typically just
            // strobe 0, where EARLY = -sps/2 is below the buffer floor)
            // fail this check; they fall through to nearest-sample
            // integer sampling instead — exactly what the batch path
            // does at `mf[0]`. With this gate, the loop integ stays
            // exactly zero across the lead-in symbols, which is the
            // right initial state.
            let farrow_ok = is_farrow_valid(early, mf.len())
                && is_farrow_valid(self.strobe_pos, mf.len())
                && is_farrow_valid(late, mf.len());
            if farrow_ok {
                let y_early = farrow::interp_complex(&mf, early);
                let y_on = farrow::interp_complex(&mf, self.strobe_pos);
                let y_late = farrow::interp_complex(&mf, late);
                // Gardner TED + PI step. The output is a fractional
                // correction (in samples-per-symbol) added to the
                // nominal sps stride. Default DVB-S2 gains give a loop
                // bandwidth ~0.01·Rs (slow, noise-robust).
                let correction = self.timing_loop.step(y_early, y_on, y_late);
                symbols.push(y_on);
                self.strobe_pos += self.sps as f64 + correction;
            } else {
                // Lead-in / FIR-warmup region: emit the integer-step
                // sample (matches batch `mf[k·sps]`) and advance by
                // exactly sps without touching the TED. The loop integ
                // remains at zero until a real Gardner step runs.
                let i = self.strobe_pos.round() as isize;
                let y = if i >= 0 && (i as usize) < mf.len() {
                    mf[i as usize]
                } else {
                    Complex64::new(0.0, 0.0)
                };
                symbols.push(y);
                self.strobe_pos += self.sps as f64;
            }
        }

        // Trim: keep `taps_half + sps + 4` audio samples BEFORE the
        // next strobe position. The taps_half part keeps the next
        // chunk's MF unbiased at the strobe (mode='same' boundary
        // bias spans `half` samples on each side). The `sps + 4` part
        // covers Farrow's 4-tap window centred on the EARLY sample of
        // the next strobe (`strobe_pos - sps/2 - 1`).
        let keep_back = self.taps_half + self.sps + 4;
        let strobe_floor = (self.strobe_pos as isize) - (keep_back as isize);
        let trim_to = strobe_floor.max(0) as usize;
        if trim_to > 0 {
            self.audio_buffer.drain(..trim_to);
            self.nco_base_sample += trim_to as u64;
            self.strobe_pos -= trim_to as f64;
        }

        symbols
    }
}

/// True when `pos` sits inside Farrow's 4-tap cubic-Lagrange window —
/// i.e. the interpolation needs `samples[n-1, n, n+1, n+2]` with
/// `n = floor(pos)`, so `1 ≤ n` and `n + 2 < len`.
#[inline]
fn is_farrow_valid(pos: f64, len: usize) -> bool {
    let n = pos.floor() as isize;
    n >= 1 && ((n + 2) as usize) < len
}

/// Downmix `samples` to baseband, using `sample_index_offset` as the
/// running NCO phase counter so chunks chain seamlessly.
///
/// `samples[i]` is mixed with carrier `exp(-j·2π·fc·(offset+i)/Fs)`.
/// Identical to [`demodulator::downmix`] when `offset == 0`.
fn downmix_with_offset(
    samples: &[f32],
    center_freq_hz: f64,
    sample_index_offset: u64,
) -> Vec<Complex64> {
    samples
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let n = sample_index_offset.wrapping_add(i as u64);
            let phase = -2.0 * PI * center_freq_hz * n as f64 / AUDIO_RATE as f64;
            Complex64::new(phase.cos(), phase.sin()) * s as f64
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_core2x::frame2x::build_superframe_v4;
    use modem_core2x::profile2x::{
        profile_high_2x, profile_normal_2x, profile_ultra_2x, ProfileIndex2x,
    };
    use modem_core_base::modulator;
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
        let symbols = build_superframe_v4(payload, cfg, session_id, mime::BINARY, 0xAA55);
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau)
                .unwrap();
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, cfg.base.center_freq_hz)
    }

    #[test]
    fn empty_chunk_returns_no_symbols() {
        let mut sf = StreamingFrontend::new(profile_high_2x());
        let out = sf.process_chunk(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn debug_compare_first_symbols_to_batch() {
        // Sanity check: with the loop *disabled* (kp=ki=0), the streaming
        // strobes must reproduce batch's `mf[k·sps]` exactly.
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0x1);
        let audio = modulate_for(&cfg, &payload, 0xABC);

        // Batch reference: downmix + MF + integer step.
        let bb = demodulator::downmix(&audio, cfg.base.center_freq_hz);
        let taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, 32);
        let mf = demodulator::matched_filter(&bb, &taps);
        let batch_syms: Vec<_> = (0..(mf.len() / 32)).map(|k| mf[k * 32]).collect();

        // Streaming with Gardner DISABLED (we just want to verify the
        // integer-step grid alignment first).
        let mut sf = StreamingFrontend::new(cfg.clone());
        sf.timing_loop = TimingLoop::new(0.0, 0.0, sf.timing_loop.ted());
        let stream_syms = sf.process_chunk(&audio);

        // First 50 symbols should match closely (FIR boundary may differ
        // by 1 symbol due to length rounding).
        let n = stream_syms.len().min(batch_syms.len()).min(50);
        for k in 0..n {
            let d = (stream_syms[k] - batch_syms[k]).norm();
            assert!(
                d < 1e-9,
                "sym {k}: stream={:?} batch={:?} diff={d}",
                stream_syms[k],
                batch_syms[k],
            );
        }
    }

    #[test]
    fn single_chunk_matches_batch_pipeline_high2x() {
        // Encode → modulate → feed audio in a single big chunk through
        // the streaming frontend. The decoder downstream should accept
        // the symbol stream identically to the batch `audio_to_symbols`
        // path (i.e. rx_v4_symbols decodes successfully).
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0xCAFE);
        let audio = modulate_for(&cfg, &payload, 0x1234);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let symbols = sf.process_chunk(&audio);
        assert!(symbols.len() > 100, "expected real symbols, got {}", symbols.len());
        let res = modem_core2x::rx_v4::rx_v4_symbols(&symbols, &cfg)
            .expect("decode through streaming frontend");
        let h = res.app_header.expect("AppHeader recovered");
        assert_eq!(h.session_id, 0x1234);
        assert_eq!(res.data, payload, "payload mismatch after streaming frontend");
    }

    #[test]
    fn chunked_equals_single_chunk_high2x() {
        // Splitting the SAME audio into 500ms chunks and feeding them
        // sequentially MUST produce the same decoded payload as a single
        // big-chunk call. Proves NCO continuity, MF tail handling, and
        // strobe-position book-keeping are correct across boundaries.
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0xBEEF);
        let audio = modulate_for(&cfg, &payload, 0xABCD);

        // Reference: single chunk.
        let mut sf_one = StreamingFrontend::new(cfg.clone());
        let syms_one = sf_one.process_chunk(&audio);
        let res_one = modem_core2x::rx_v4::rx_v4_symbols(&syms_one, &cfg).unwrap();

        // Streaming: split audio in 24 000-sample pieces.
        let mut sf_chunked = StreamingFrontend::new(cfg.clone());
        let mut syms_chunked = Vec::new();
        const CHUNK: usize = 24_000;
        for i in (0..audio.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(audio.len());
            syms_chunked.extend(sf_chunked.process_chunk(&audio[i..end]));
        }

        // Symbol counts may differ by ≤ 1 because of where the strobe
        // falls relative to the buffer trim (the chunked path may not
        // yet have emitted the last symbol when the audio ends). What
        // matters is that the decoded payload is byte-identical.
        let diff = (syms_one.len() as isize - syms_chunked.len() as isize).abs();
        assert!(diff <= 2, "symbol count drift across chunking: {diff}");

        let res_chunked = modem_core2x::rx_v4::rx_v4_symbols(&syms_chunked, &cfg).unwrap();
        let h_one = res_one.app_header.unwrap();
        let h_chunked = res_chunked.app_header.unwrap();
        assert_eq!(h_one.session_id, h_chunked.session_id);
        assert_eq!(res_one.data, res_chunked.data, "chunked decode != single decode");
        assert_eq!(res_chunked.data, payload);
    }

    #[test]
    fn chunked_normal_2x_decodes_correctly() {
        // NORMAL2X has the smallest sps (probably 32 or 48 depending on
        // profile rate) — different from HIGH2X — make sure the
        // streaming path is profile-agnostic.
        let cfg = profile_normal_2x();
        let payload = rng_bytes(400, 0x11);
        let audio = modulate_for(&cfg, &payload, 0x99);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let mut syms = Vec::new();
        const CHUNK: usize = 18_000;
        for i in (0..audio.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(audio.len());
            syms.extend(sf.process_chunk(&audio[i..end]));
        }
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg).expect("decode");
        assert_eq!(res.data, payload);
    }

    #[test]
    fn chunked_ultra_2x_decodes_correctly() {
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(150, 0xDE);
        let audio = modulate_for(&cfg, &payload, 0xCAFE);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let mut syms = Vec::new();
        const CHUNK: usize = 36_000;
        for i in (0..audio.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(audio.len());
            syms.extend(sf.process_chunk(&audio[i..end]));
        }
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg).expect("decode");
        assert_eq!(res.data, payload);
    }

    #[test]
    fn timing_loop_tracks_resampled_audio_50ppm() {
        // Resample the modulated audio to simulate a +50 ppm clock drift
        // between TX and RX, then feed it through the streaming
        // frontend. Without Gardner the integer-step sampler would walk
        // off the strobe; with it the symbol stream still decodes.
        let cfg = profile_high_2x();
        let payload = rng_bytes(300, 0xF0);
        let audio = modulate_for(&cfg, &payload, 0x77);
        let resampled = resample_linear(&audio, 1.0 + 50e-6);
        let mut sf = StreamingFrontend::new(cfg.clone());
        let mut syms = Vec::new();
        const CHUNK: usize = 24_000;
        for i in (0..resampled.len()).step_by(CHUNK) {
            let end = (i + CHUNK).min(resampled.len());
            syms.extend(sf.process_chunk(&resampled[i..end]));
        }
        let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg)
            .expect("decode 50 ppm-drifted audio");
        // TimingLoop integ should have converged near 50 ppm × sps —
        // i.e. roughly `sps · 50e-6` extra samples-per-symbol. Sanity-
        // check it's positive (we resampled UP, so the "true" strobe
        // step is slightly larger than nominal sps).
        let integ = sf.timing_loop().integ();
        assert!(integ > 0.0, "integ should be positive for +ppm, got {integ}");
        assert_eq!(res.data, payload);
    }

    /// Crude linear resampler used only to inject a constant ppm offset
    /// into the test signal. Far less precise than a real timing-
    /// recovered pipeline — the point is to give the StreamingFrontend's
    /// Gardner loop a non-zero error signal to track.
    fn resample_linear(input: &[f32], ratio: f64) -> Vec<f32> {
        let out_len = ((input.len() as f64) * ratio) as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let src = (i as f64) / ratio;
            let i0 = src.floor() as usize;
            let i1 = (i0 + 1).min(input.len() - 1);
            let frac = src - i0 as f64;
            let v = input[i0] as f64 * (1.0 - frac) + input[i1] as f64 * frac;
            out.push(v as f32);
        }
        out
    }

    #[test]
    fn all_profiles_decode_through_streaming_frontend() {
        // Belt-and-braces: every shipped 2x profile must survive the
        // streaming path on a 200-byte payload.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(200, p.as_u8() as u64);
            let audio = modulate_for(&cfg, &payload, 0x42);
            let mut sf = StreamingFrontend::new(cfg.clone());
            let mut syms = Vec::new();
            const CHUNK: usize = 24_000;
            for i in (0..audio.len()).step_by(CHUNK) {
                let end = (i + CHUNK).min(audio.len());
                syms.extend(sf.process_chunk(&audio[i..end]));
            }
            let res = modem_core2x::rx_v4::rx_v4_symbols(&syms, &cfg)
                .unwrap_or_else(|| panic!("{p:?} streaming decode returned None"));
            assert_eq!(res.data, payload, "{p:?} streaming roundtrip");
        }
    }
}
