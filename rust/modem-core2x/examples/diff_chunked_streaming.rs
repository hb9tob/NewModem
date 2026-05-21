//! Bit-equivalence probe — feed the same WAV through Rx2xSession twice
//! with two different chunk sizes and report the first absolute symbol
//! index where the two `sym_buffer`s disagree.
//!
//! Usage: diff_chunked_streaming <profile> <wav> [chunk_a] [chunk_b]
//!   defaults: chunk_a=2400, chunk_b=24000

use modem_core2x::profile2x::ProfileIndex2x;
use modem_core2x::rx2x_session::Rx2xSession;
use num_complex::Complex64;

fn read_wav_f32(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("read wav");
    // Skip 44-byte canonical PCM header, 16-bit mono assumed.
    bytes[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

fn run(profile: ProfileIndex2x, samples: &[f32], chunk: usize) -> (u64, Vec<Complex64>) {
    let cfg = profile.to_config();
    let mut s = Rx2xSession::new(cfg, profile.name().to_string());
    for i in (0..samples.len()).step_by(chunk) {
        let end = (i + chunk).min(samples.len());
        s.process_audio_chunk(&samples[i..end]);
    }
    // Drain finalize events but ignore them.
    let _ = s.finalize();
    // We need read access to sym_buffer + buf_start_abs. The public
    // accessors live on Rx2xSession; if not exposed we panic with a
    // hint and the user adds them.
    (s.buf_start_abs_pub(), s.sym_buffer_pub().to_vec())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let profile_name = args.next().expect("profile arg");
    let wav = args.next().expect("wav arg");
    let chunk_a: usize = args
        .next()
        .map(|s| s.parse().unwrap())
        .unwrap_or(2400);
    let chunk_b: usize = args
        .next()
        .map(|s| s.parse().unwrap())
        .unwrap_or(24000);

    let profile = ProfileIndex2x::from_name(&profile_name).expect("profile");
    let samples = read_wav_f32(&wav);
    println!(
        "wav: {} ({} samples, {:.2}s)  profile: {}  chunks: A={} B={}",
        wav,
        samples.len(),
        samples.len() as f64 / 48000.0,
        profile.name(),
        chunk_a,
        chunk_b,
    );

    let (start_a, buf_a) = run(profile, &samples, chunk_a);
    let (start_b, buf_b) = run(profile, &samples, chunk_b);
    println!(
        "A: start_abs={} len={}    B: start_abs={} len={}",
        start_a,
        buf_a.len(),
        start_b,
        buf_b.len(),
    );

    let overlap_start = start_a.max(start_b);
    let end_a = start_a + buf_a.len() as u64;
    let end_b = start_b + buf_b.len() as u64;
    let overlap_end = end_a.min(end_b);
    if overlap_end <= overlap_start {
        println!("no overlap — buffers cover disjoint absolute ranges");
        return;
    }
    println!(
        "overlap absolute [{}..{}]  ({} symbols)",
        overlap_start,
        overlap_end,
        overlap_end - overlap_start,
    );

    // Per-symbol diff. Report the first 8 absolute positions where
    // |A - B| > eps, plus the max diff and its position.
    let mut first_diffs: Vec<(u64, Complex64, Complex64, f64)> = Vec::new();
    let mut max_err = 0.0f64;
    let mut max_at = overlap_start;
    let eps = 1e-9;
    for abs in overlap_start..overlap_end {
        let a = buf_a[(abs - start_a) as usize];
        let b = buf_b[(abs - start_b) as usize];
        let e = (a - b).norm();
        if e > max_err {
            max_err = e;
            max_at = abs;
        }
        if e > eps && first_diffs.len() < 8 {
            first_diffs.push((abs, a, b, e));
        }
    }
    if first_diffs.is_empty() {
        println!("✔ buffers are bit-equivalent (max err = {:.2e})", max_err);
    } else {
        println!("✘ buffers DIVERGE — first {} mismatches:", first_diffs.len());
        for (abs, a, b, e) in &first_diffs {
            println!(
                "  abs={} A=({:+.6},{:+.6}) B=({:+.6},{:+.6}) err={:.4e}",
                abs, a.re, a.im, b.re, b.im, e,
            );
        }
        println!(
            "max err = {:.4e} at abs={}",
            max_err, max_at,
        );
    }
}
