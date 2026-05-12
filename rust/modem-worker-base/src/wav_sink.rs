//! Tee-the-raw-capture WAV sink, shared between V3 and V4 RX workers.
//!
//! The Tauri command `start_raw_recording` constructs a [`WavSink`] in
//! the GUI process and stores it in [`SharedWavSink`]. The active RX
//! worker then writes every ingested f32 batch into the same sink while
//! the slot holds `Some`. `stop_raw_recording` flips the slot back to
//! `None` and finalises the writer, producing a play-anywhere mono
//! 48 kHz 16-bit WAV that lets users post-mortem an OTA-captured burst.
//!
//! No DSP in here — just the file-format glue. The format choice (16-bit
//! signed integer, 48 kHz, mono) matches every other WAV the modem reads
//! / writes (CLI, GUI playback, regression tests), so no transcoding
//! is needed across the pipeline.

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hound::{SampleFormat, WavSpec, WavWriter};
use modem_core_base::types::AUDIO_RATE;

type WavFileWriter = WavWriter<BufWriter<std::fs::File>>;

/// Open WAV file + running sample counter. The worker writes into this
/// while [`SharedWavSink`] holds a `Some`; the Tauri start/stop commands
/// create and finalize it.
pub struct WavSink {
    writer: WavFileWriter,
    pub path: PathBuf,
    pub samples_written: u64,
}

impl WavSink {
    /// Create a new 48 kHz mono 16-bit WAV at `path`.
    pub fn create(path: &Path) -> Result<Self, hound::Error> {
        let spec = WavSpec {
            channels: 1,
            sample_rate: AUDIO_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer = WavWriter::create(path, spec)?;
        Ok(Self {
            writer,
            path: path.to_path_buf(),
            samples_written: 0,
        })
    }

    /// Append `samples` to the WAV, converting from f32 `[-1, 1]` to
    /// 16-bit signed PCM with hard clamping. Called by both the V3 and
    /// V4 workers' ingest loop on every received batch.
    pub fn write_chunk(&mut self, samples: &[f32]) {
        for &s in samples {
            let val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            let _ = self.writer.write_sample(val);
        }
        self.samples_written += samples.len() as u64;
    }

    /// Flush + write header size. Consumes self.
    pub fn finalize(self) -> Result<(PathBuf, u64), hound::Error> {
        let samples = self.samples_written;
        let path = self.path.clone();
        self.writer.finalize()?;
        Ok((path, samples))
    }
}

/// Shared raw-capture sink. `None` = not recording ; `Some` = worker is
/// teeing every ingested batch into the WAV.
pub type SharedWavSink = Arc<Mutex<Option<WavSink>>>;
