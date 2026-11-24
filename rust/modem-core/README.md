# modem-core

Transport layer of the NBFM modem — DSP and superframe assembly. Owns
constellations (QPSK, 8PSK, 16/32/64-APSK DVB-S2X), root-raised cosine
shaping, LDPC WiMAX (IEEE 802.16e, N=2304), interleaver, FFE T/2 +
DD-PLL receiver, marker-based sync, and the V4 superframe layout
(headerless, marker-bootstrap; built by `build_superframe_v3`).

## Public API

- `Modem` trait + `V3Modem` (the only impl today). `V3Modem::list_profiles()`
  is the authoritative source of available modes.
- `ProfileIndex::ALL` + `profile::config_by_name(name)` — single source of
  truth when adding a new mode.
- `V3Modem::encode_to_samples(EncodeRequest) -> Vec<f32>` for in-process
  TX (no subprocess).
- `rx_v2::*` for the sliding-window RX pipeline.

Depends on `modem-framing` for envelope / app header / RaptorQ / CRC. Does
not pull in any audio device crate (cpal lives in `modem-io`).

## What stays here vs goes elsewhere

- DSP, FEC, modulation, sync, V4 superframe → here.
- Payload envelope, app header, RaptorQ codec, CRC → `modem-framing`
  (transport-agnostic, reusable over non-RF transports).
- Audio device IO → `modem-io`.
- TX/RX worker threads, session persistence, PTT → `modem-worker`.

The wire format is the **V4 headerless layout** (since 0.14.0): the protocol
header is dropped and its `profile_index` lives in the bootstrap marker, which
is followed by the LMS warmup, then the data — burst layout
`PREAMBLE → MARKER(profile_index, META) → WARMUP → META_CW → [MARKER → 2·CW]*`.
The marker payload carries `profile_index` + `fmt_version` (= `WIRE_FMT_V4`) +
an `EOT_FRAME` flag. Hard break with V3 (≤ 0.13.18): both ends must run V4.
The format is frozen bit-for-bit on `main`; do not change it without bumping
`fmt_version` and updating the OTA-deployed receivers. See
`rust/V4_WIRE_FORMAT_PLAN.md`.
