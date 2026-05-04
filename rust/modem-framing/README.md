# modem-framing

Transport-agnostic framing utilities — no DSP, no audio IO. Reusable
across modem families and over non-RF transports (UDP, file, …).

## Modules

- `payload_envelope` — `PayloadEnvelope` (filename + callsign + content
  + magic) used as the raw payload before fountain coding. Includes
  `decode_or_fallback` for legacy un-enveloped payloads.
- `app_header` — `AppHeader` (file size, hash, MIME, RaptorQ params,
  session_id), Golay-protected on the wire. `mime::*` constants for
  AVIF / JPEG / PNG / ZSTD / BINARY.
- `raptorq_codec` — fountain encode / decode wrapper around `raptorq`.
  `k_from_payload`, `n_repair_default`, `encode_packets_range`,
  `try_decode`.
- `crc` — CRC-8 / CRC-16 used by `app_header` and the V3 superframe
  marker. Lives here because `app_header::crc16` needs it and
  `modem-framing` cannot depend on `modem-core` (would create a cycle).

## Why a separate crate

Every byte that crosses the air also makes sense over UDP, a file, or a
future Pluto-IF transport. Keeping the framing code free of DSP
dependencies lets a second modem family (planned: QO-100 NB transponder)
reuse it verbatim without dragging in cpal / LDPC / RRC.

No `FramingLayer` trait yet — there is only one concrete framing today.
The trait will be extracted when a second call-site appears, to avoid
speculative API.
