//! In-process TX worker for the 2x family.
//!
//! Wraps [`V4Modem::encode_to_samples`] with a few orchestration helpers
//! the GUI / CLI need: write the audio to a WAV file, push it to a
//! `SampleSink` for live playback, or hand it back as a buffer for
//! further processing.
//!
//! No threading lives here — the caller decides whether to run the
//! encode on a background thread (the GUI's TX command spawns one,
//! the CLI runs it inline). Keeping this synchronous keeps the unit
//! tests fast and avoids leaking lifetime constraints into the trait
//! boundary.

use std::path::Path;

use hound::{SampleFormat, WavSpec, WavWriter};
use modem_core_base::traits::{EncodeRequest, Modem, ModemError};
use modem_core_base::types::AUDIO_RATE;
use modem_core2x::modem2x::V4Modem;

/// Errors surfaced by the TX worker.
#[derive(Debug)]
pub enum TxError {
    /// The modem rejected the request (unknown profile, invalid
    /// `(symbol_rate, tau)` combination, ...).
    Modem(ModemError),
    /// File I/O failed when writing a WAV.
    Wav(hound::Error),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Modem(e) => write!(f, "modem error: {e}"),
            Self::Wav(e) => write!(f, "wav write error: {e}"),
        }
    }
}

impl std::error::Error for TxError {}

impl From<ModemError> for TxError {
    fn from(e: ModemError) -> Self { Self::Modem(e) }
}
impl From<hound::Error> for TxError {
    fn from(e: hound::Error) -> Self { Self::Wav(e) }
}

/// Encode an [`EncodeRequest`] to mono 48 kHz `f32` audio samples.
///
/// Thin pass-through over [`V4Modem::encode_to_samples`]; exposed here so
/// every TX path (GUI, CLI, tests) goes through one entry point and the
/// future `tx_worker_base` factoring stays trivial.
pub fn encode_to_audio(req: &EncodeRequest<'_>) -> Result<Vec<f32>, TxError> {
    Ok(V4Modem.encode_to_samples(req)?)
}

/// Encode an [`EncodeRequest`] and write the resulting audio to a WAV
/// file at `path`. Mono, 48 kHz, 32-bit float — the format the GUI's
/// playback path reads back without conversion.
///
/// Returns the number of samples actually written.
pub fn encode_to_wav(req: &EncodeRequest<'_>, path: &Path) -> Result<usize, TxError> {
    let samples = encode_to_audio(req)?;
    let spec = WavSpec {
        channels: 1,
        sample_rate: AUDIO_RATE,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(path, spec)?;
    for s in &samples {
        writer.write_sample(*s)?;
    }
    writer.finalize()?;
    Ok(samples.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_framing::app_header::mime;
    use tempfile::NamedTempFile;

    fn req() -> EncodeRequest<'static> {
        EncodeRequest {
            profile: "HIGH2X",
            wire_payload: b"hello modem 2x",
            session_id: 0xCAFE_BABE,
            mime_type: mime::BINARY,
            hash_short: 0xAA55,
            esi_start: 0,
            n_packets: 6,
            vox_seconds: 0.0,
        }
    }

    #[test]
    fn encode_to_audio_returns_non_empty_buffer() {
        let audio = encode_to_audio(&req()).expect("encode");
        assert!(!audio.is_empty());
        let max_abs = audio.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
        assert!(max_abs > 0.05);
        assert!(max_abs <= 1.0);
    }

    #[test]
    fn encode_to_audio_unknown_profile_fails() {
        let mut r = req();
        r.profile = "NOPE";
        match encode_to_audio(&r).unwrap_err() {
            TxError::Modem(ModemError::UnknownProfile(_)) => {}
            other => panic!("expected UnknownProfile, got {other}"),
        }
    }

    #[test]
    fn encode_to_wav_writes_readable_file() {
        let tmp = NamedTempFile::new().expect("tmp");
        let path = tmp.path().to_path_buf();
        let n = encode_to_wav(&req(), &path).expect("write wav");
        assert!(n > 0);
        // Re-read the WAV and verify it has the expected header.
        let reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, AUDIO_RATE);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, SampleFormat::Float);
        // Sample count matches what encode_to_audio returned.
        let n_samples = reader.duration() as usize;
        assert_eq!(n_samples, n);
    }
}
