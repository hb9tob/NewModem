# modem-core

Transport layer of the NBFM modem — DSP and superframe assembly. Owns
constellations (QPSK, 8PSK, 16/32/64-APSK DVB-S2X), root-raised cosine
shaping, LDPC WiMAX (IEEE 802.16e, N=2304), interleaver, FFE T/2 +
DD-PLL receiver, marker-based sync, and the V3 superframe layout.

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

- DSP, FEC, modulation, sync, V3 superframe → here.
- Payload envelope, app header, RaptorQ codec, CRC → `modem-framing`
  (transport-agnostic, reusable over non-RF transports).
- Audio device IO → `modem-io`.
- TX/RX worker threads, session persistence, PTT → `modem-worker`.

The wire format (V3 superframe + AppHeader + envelope) is frozen
bit-for-bit on `main`. Do not change it without bumping the protocol
version and updating the OTA-deployed receivers.
