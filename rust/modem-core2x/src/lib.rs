//! NBFM audio modem — **2x PHY** (DVB-S2X-inspired wire format V4).
//!
//! Parallel to [`modem_core`] (V3). Both crates depend on
//! [`modem_core_base`] for the shared DSP primitives (constellations,
//! LDPC, RRC, Golay, FFE, PLL, soft demodulation, Farrow interpolator,
//! Gardner closed-loop timing recovery). 2x adds a fresh frame layout
//! and RX pipeline on top, NOT a re-implementation of the lower layers.
//!
//! # What changes vs V3
//!
//! - **PLHEADER** (192 sym) replaces preamble + LMS warmup + header
//!   + per-segment marker triplet.
//! - **Sparse pilot blocks** (36 sym, value `P = (1+j)/√2`, one block
//!   after every LDPC codeword; densified to 2 blocks/CW for the
//!   APSK-32 and APSK-64 profiles only) replace the V3 TDM pilots
//!   interleaved inside each segment.
//! - **Closed-loop timing recovery** via
//!   `modem_core_base::timing_loop::TimingLoop` + Farrow interpolation
//!   replaces the V3 open-loop Phase 1d marker-correlation drift
//!   estimator + bulk `resample_audio`.
//!
//! # What stays
//!
//! - LDPC WiMAX 802.16e N=2304, 4 rates — unchanged.
//! - DVB-S2/S2X constellations (QPSK, 8PSK, 16/32/64-APSK) — unchanged.
//! - RaptorQ fountain code (in `modem_framing::raptorq_codec`) —
//!   unchanged. ESI tracking moves from marker control payload to PLS
//!   payload but the codec API is identical.
//! - `AppHeader` meta codeword carrying session_id, file_size, K, T,
//!   mime, hash — emitted right after each PLHEADER, replicated 4×
//!   inside one LDPC codeword like in V3.
//! - `Modem` trait + `EncodeRequest` / `ProfileDescriptor` interfaces
//!   from `modem_core_base::traits` — `V4Modem` implements them.
//!
//! # Mutual exclusion with V3
//!
//! At runtime, the worker is either in **legacy (V3)** mode using
//! `modem_worker` + `modem_core::v3_modem::V3Modem`, **or** in **2x
//! (V4)** mode using `modem_worker2x` + `modem_core2x::V4Modem`. Never
//! both at once. The choice is a CLI flag / GUI top-level selector and
//! is fixed for the session — see the plan in
//! `~/.claude/plans/je-voudrsis-edudier-la-precious-treasure.md`.
//!
//! # Module roadmap (filled in by Phase C-1 onward)
//!
//! - `plheader` — Start-of-Frame + PLS encoding/decoding.
//! - `pilot_block` — 36-sym `(1+j)/√2` blocks + interleaver.
//! - `profile2x` — 8 profiles `*2x` incl. `HighPlusPlus2x`.
//! - `frame2x` — `build_superframe_v4`.
//! - `rx_v4` — full RX pipeline, integrates Farrow + TimingLoop.
//! - `gate2x` + `detect2x` — FFT-gate + auto-detect 2x.
//! - `modem2x` — `V4Modem` impl of the `Modem` trait.

// Phase C-1 — PLHEADER (192 sym SOF+PLS) and TDM pilots (V3-style
// rotating-QPSK groups of `d_syms` data + `p_syms` pilots).
// Both are isolated frame primitives: no profile or frame-builder
// dependency.
pub mod pilot2x_tdm;
pub mod plheader;

// Phase C-2 — `ProfileIndex2x` enum (8 profiles, HighPlusPlus2x
// promoted) and `ModemConfig2x` struct used by the encoder/decoder.
pub mod profile2x;

// Phase C-3 — V4 superframe builder. Wires PLHEADER + LMS warmup +
// META-CW + (pilot_block + DATA-CW)* into the wire stream of complex
// symbols. Mutually exclusive with V3 frame layout (no markers, no
// TDM intra-CW pilots, no runout).
pub mod frame2x;

// Phase C-4 — V4 receive pipeline (symbol domain). The audio-domain
// matched filter + Farrow + TimingLoop wrapper lives in the worker
// (Phase C-7); rx_v4 here decodes a stream of complex symbols already
// sampled at the symbol rate.
pub mod rx_v4;

// Phase C-5 — FFT-based SOF presence probe. Cheap idle gate that lets
// the worker skip the symbol-domain SOF correlation when the audio
// buffer holds nothing but band noise. Three templates by (sps, β)
// bucket cover the 8 ProfileIndex2x entries; PLS payload of the
// matching cycle refines the anchor profile downstream.
pub mod gate2x;

// Phase C-6 — `V4Modem` impl of the `Modem` trait from
// modem-core-base::traits. Stateless wrapper that maps a profile name
// to its config, calls frame2x + modulator, returns audio samples.
pub mod modem2x;

// Slice 2x19 — streaming audio→symbol front-end (was in modem-worker2x
// pre-2x19 but architecturally belongs in core: it's pure DSP and the
// live session machine that depends on it lives here too).
pub mod streaming_frontend;

// Slice 2x23 — streaming DSP pipeline (polyphase FIR resampler + NCO
// downmix + overlap-save matched filter + decimation). Each stage
// keeps its own state across `feed_audio` calls so chunk boundaries
// leave NO residual edge effect; every audio sample is processed
// exactly once and symbols emerge from a single continuous stream.
// Replaces the per-chunk `refresh_symbols` rebuild that was causing
// MF-edge "clicks" and a σ² blow-up of ~38 dB on static drift in the
// channel-sim validation 2026-05-17.
pub mod streaming_dsp;

// Slice 2x19 — live streaming RX session. Full state machine + turbo
// loops integrated. Replaces the worker's batch decode pattern. See
// plan `ok-alors-le-rms-precious-shannon.md`.
pub mod rx2x_session;
