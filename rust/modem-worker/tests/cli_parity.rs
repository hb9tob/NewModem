//! Bit-for-bit parity test between the legacy `nbfm-modem tx` CLI and
//! the in-process `V3Modem::encode_to_samples` implementation introduced
//! in phase 2D-i.
//!
//! The test runs the CLI on a deterministic 200-byte payload, reads the
//! WAV samples it produced (i16 PCM mono 48 kHz), and compares them to
//! what V3Modem produces in-process for the *exact same* EncodeRequest.
//! After the same `f32 -> i16` quantization the WAV writer applies, the
//! two streams must be byte-identical — any drift means we've broken
//! wire compatibility with deployed receivers.
//!
//! Requires the CLI binary to exist at `target/release/nbfm-modem[.exe]`
//! or `target/debug/nbfm-modem[.exe]`. Run
//! `cargo build -p modem-cli --release` first.

use std::path::PathBuf;
use std::process::Command;

use modem_core::frame::effective_packet_count;
use modem_core::ldpc::encoder::LdpcEncoder;
use modem_core::payload_envelope::PayloadEnvelope;
use modem_core::profile::ProfileIndex;
use modem_core::raptorq_codec::k_from_payload;
use modem_core::traits::{EncodeRequest, Modem};
use modem_core::v3_modem::V3Modem;

/// Locate the CLI binary in the workspace's target directory. Looks for
/// the release build first (matches what the GUI's portable packaging
/// ships), falls back to debug. The integration test panics with a
/// helpful message if neither is present.
fn nbfm_modem_path() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent()?;
    let exe_name = if cfg!(windows) { "nbfm-modem.exe" } else { "nbfm-modem" };
    for profile in ["release", "debug"] {
        let p = workspace.join("target").join(profile).join(exe_name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// FNV-1a + xor-fold to 16 bits — same routine the CLI uses to seed the
/// app-header `hash_short` field (see `content_hash_short` in
/// modem-cli/src/main.rs).
fn fnv_short(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

#[test]
fn cli_and_v3modem_produce_identical_samples() {
    let cli = nbfm_modem_path().expect(
        "nbfm-modem binary not found in target/{release,debug}/. \
         Run `cargo build -p modem-cli --release` before this test.",
    );

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let payload_path = tmp.path().join("payload.bin");
    let wav_path = tmp.path().join("out.wav");

    // 200-byte deterministic payload — small enough for a fast test
    // (~ a couple of seconds of audio on NORMAL), large enough to cover
    // multiple LDPC codewords + the app-header meta segment + EOT.
    let mut payload = vec![0u8; 200];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = ((i.wrapping_mul(31)).wrapping_add(7)) as u8;
    }
    std::fs::write(&payload_path, &payload).expect("write payload");

    let profile_name = "NORMAL";
    let callsign = "HB9TEST";
    let filename = "payload.bin";
    let repair_pct: u32 = 5;
    let session_id: u32 = 0xDEAD_BEEF;

    let status = Command::new(&cli)
        .arg("tx")
        .args(["--input", &payload_path.to_string_lossy()])
        .args(["--output", &wav_path.to_string_lossy()])
        .args(["--profile", profile_name])
        .args(["--callsign", callsign])
        .args(["--filename", filename])
        .args(["--repair-pct", &repair_pct.to_string()])
        .args(["--session-id", &format!("{session_id:08X}")])
        .status()
        .expect("spawn nbfm-modem");
    assert!(status.success(), "nbfm-modem tx exited with {status:?}");

    let mut reader = hound::WavReader::open(&wav_path).expect("open WAV");
    let cli_samples: Vec<i16> = reader
        .samples::<i16>()
        .map(|s| s.expect("sample read"))
        .collect();

    // Reproduce the CLI's exact wire-payload assembly (cf. modem-cli/src/main.rs
    // around lines 233-280): envelope wrap -> RaptorQ K planning -> n_total.
    let envelope =
        PayloadEnvelope::new(filename, callsign, payload.clone()).expect("envelope");
    let wire_payload = envelope.encode();

    let pi = ProfileIndex::Normal;
    let cfg = pi.to_config();
    let k_bytes = LdpcEncoder::new(cfg.ldpc_rate).k() / 8;
    let k_src = k_from_payload(wire_payload.len(), k_bytes) as u32;
    let n_total = effective_packet_count(k_src + (k_src * repair_pct) / 100);

    let req = EncodeRequest {
        profile: profile_name,
        wire_payload: &wire_payload,
        session_id,
        mime_type: modem_core::app_header::mime::BINARY,
        hash_short: fnv_short(&wire_payload),
        esi_start: 0,
        n_packets: n_total,
        // CLI default --vox 0.5 — keep parity until the GUI worker
        // explicitly opts into vox=0 (PTT covers the same role).
        vox_seconds: 0.5,
    };
    let v3_samples = V3Modem.encode_to_samples(&req).expect("v3 encode");

    // Same `f32 -> i16` quantization the CLI's `write_wav` applies.
    let v3_i16: Vec<i16> = v3_samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();

    assert_eq!(
        v3_i16.len(),
        cli_samples.len(),
        "sample-count mismatch: V3 produced {} samples, CLI {}",
        v3_i16.len(),
        cli_samples.len(),
    );

    if let Some((idx, (a, b))) = v3_i16
        .iter()
        .zip(cli_samples.iter())
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        panic!(
            "sample mismatch at index {idx}/{}: V3={a} CLI={b}",
            v3_i16.len(),
        );
    }
}
