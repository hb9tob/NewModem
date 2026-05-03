//! Transport-agnostic framing utilities for the NBFM modem stack.
//!
//! Phase 4 of the layered-arch refactor extracts these modules from
//! `modem-core` so a future modem family (e.g. QO-100) and non-RF
//! transports (e.g. UDP) can reuse them without depending on the V3
//! NBFM modem internals:
//!
//! - [`payload_envelope`] — fixed-format wrap of user file bytes
//!   (filename + callsign + content). Bidirectional encode/decode.
//! - [`app_header`] — session_id, mime types and short content hash
//!   computed before any FEC. Identifies a payload across retries.
//! - [`raptorq_codec`] — RFC 6330 fountain code (single source-block,
//!   ESI = linear packet index). Deterministic encoder over (bytes,
//!   T, ESI range), decoder accumulating packets until K is reached.
//!
//! No code from these modules references LDPC / RRC / pilots / symbols
//! — they are pure byte-stream operations. Wire format is byte-for-byte
//! identical to what `modem-core` produced before the move; existing V3
//! receivers stay compatible without recompiling.

pub mod app_header;
pub mod crc;
pub mod payload_envelope;
pub mod raptorq_codec;
