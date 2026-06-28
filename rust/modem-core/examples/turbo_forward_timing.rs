//! Worst-case FORWARD decode cost of the turbo streaming RX, measured on the
//! host. Answers: "if every turbo loop fires (no codeword converges), how much
//! wall-time does decoding one second of audio cost?" i.e. the real-time factor
//! `f = wall_seconds / audio_seconds` for the forward path (turbo passes + retry,
//! NOT the one-shot worker-level replays).
//!
//! Method: build a clean multi-superframe V3 burst, add white Gaussian noise at a
//! range of levels, drive a fresh `V3Session` with `process_audio_chunk` as a
//! sample stream, and time it. As noise rises the markers still acquire but the
//! data LDPC stops converging — the band where `canon_demod_segment` runs the
//! full `V3_TURBO_SOFT_ITERS` passes AND `retry_failed_segments` re-runs each
//! failing segment up to `MAX_RETRY_ATTEMPTS` times. The peak `f` over the sweep
//! is the worst-case forward cost.
//!
//! Run (Ryzen, release-fast):
//!   cargo run -p modem-core --profile release-fast --example turbo_forward_timing

use std::time::Instant;

use modem_core::profile::ProfileIndex;
use modem_core::types::{AUDIO_RATE, RRC_SPAN_SYM};
use modem_core::v3_session::{V3Session, V3SessionEvent};
use modem_core::{frame, modulator, rrc as rrc_mod};

/// Box-Muller Gaussian from a cheap LCG (no external rng dependency).
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32 / (1u64 << 31) as f32) - 1.0 // ~U(-1,1)
    }
    fn next_gauss(&mut self) -> f32 {
        // Two uniforms in (0,1].
        let u1 = (self.next_f32() * 0.5 + 0.5).max(1e-7);
        let u2 = self.next_f32() * 0.5 + 0.5;
        (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
    }
}

fn build_burst(cfg: &modem_core::profile::ModemConfig, payload_bytes: usize) -> Vec<f32> {
    let payload = vec![0x5Au8; payload_bytes];
    let n_packets = ((payload_bytes + 31) / 32) as u32;
    let symbols =
        frame::build_superframe_v3_range(&payload, cfg, 0x1234_5678, 0x01, 0x1234, 0, n_packets);
    let (sps, pitch) =
        rrc_mod::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
    let taps = rrc_mod::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
    modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz)
}

fn signal_std(x: &[f32]) -> f32 {
    let n = x.len() as f64;
    let mean = x.iter().map(|&v| v as f64).sum::<f64>() / n;
    let var = x.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n;
    var.sqrt() as f32
}

/// Drive a fresh session over the noisy burst as an irregular stream; return
/// (wall_seconds, audio_seconds, converged_data_cw, total_data_cwdecoded_events).
fn time_decode(profile: ProfileIndex, noisy: &[f32]) -> (f64, f64, usize, usize) {
    let cfg = profile.to_config();
    let mut session = V3Session::new(cfg, profile.name().to_string());
    let mut converged = 0usize;
    let mut decoded_events = 0usize;
    let sizes = [2400usize, 997, 4096, 480, 1500];
    let t0 = Instant::now();
    let mut i = 0;
    let mut k = 0;
    while i < noisy.len() {
        let n = sizes[k % sizes.len()].min(noisy.len() - i);
        for e in session.process_audio_chunk(&noisy[i..i + n]) {
            if let V3SessionEvent::CwDecoded {
                is_meta: false,
                converged: c,
                ..
            } = e
            {
                decoded_events += 1;
                if c {
                    converged += 1;
                }
            }
        }
        i += n;
        k += 1;
    }
    let _ = session.finalize();
    let wall = t0.elapsed().as_secs_f64();
    let audio = noisy.len() as f64 / AUDIO_RATE as f64;
    (wall, audio, converged, decoded_events)
}

fn main() {
    let profiles = [
        ProfileIndex::Normal,
        ProfileIndex::High,
        ProfileIndex::HighPlusPlus,
        ProfileIndex::Robust,
        ProfileIndex::Ultra,
    ];
    // Noise as a multiple of the clean-signal std. 0 = clean (best case, T≈1);
    // the worst case is the highest multiple where markers still acquire.
    let noise_mults = [0.0f32, 0.25, 0.4, 0.55, 0.7, 0.85, 1.0, 1.2];
    // ~12 s of audio per profile so several superframes are averaged.
    let payload_bytes = 3000usize;

    println!(
        "host worst-case FORWARD turbo decode cost  (f = wall/audio; peak = all loops firing)\n\
         profile         noise×σ   audio_s   wall_s    f(=wall/audio)   data_cw conv/decoded"
    );
    for &p in &profiles {
        let cfg = p.to_config();
        let cw_sf = frame::data_cw_per_superframe(&cfg);
        let clean = build_burst(&cfg, payload_bytes);
        let sig = signal_std(&clean);
        let mut peak_f = 0.0f64;
        for &m in &noise_mults {
            let mut rng = Lcg(0xDEADBEEF ^ ((m * 1000.0) as u64).wrapping_mul(2654435761));
            let mut noisy = clean.clone();
            let sigma = sig * m;
            for v in noisy.iter_mut() {
                *v += sigma * rng.next_gauss();
            }
            let (wall, audio, conv, dec) = time_decode(p, &noisy);
            let f = wall / audio;
            peak_f = peak_f.max(f);
            println!(
                "{:<14}  {:>5.2}    {:>7.2}  {:>7.3}   {:>8.4}        {:>4}/{:<4}",
                p.name(),
                m,
                audio,
                wall,
                f,
                conv,
                dec,
            );
        }
        println!(
            "  -> {:<10} PEAK f = {:.4}  ({} data CW/SF; SF=4s; cycle={:.2}s)\n",
            p.name(),
            peak_f,
            cw_sf,
            // one marker cycle in seconds (cycle_samples / AUDIO_RATE) via a throwaway session
            V3Session::new(p.to_config(), p.name().to_string()).cycle_samples() as f64
                / AUDIO_RATE as f64,
        );
    }
}
