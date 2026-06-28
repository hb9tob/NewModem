//! Micro-benchmark: current scalar `decode_soft` vs the new contiguous
//! `decode_soft_grouped` (layout-only win, before any SIMD), on a
//! non-converging input so both run the full 50-iteration budget — the
//! worst-case / noise-limit cost that dominates when nothing decodes.
//!
//!   cargo run -p modem-core --profile release-fast --example ldpc_bench

use std::time::Instant;

use modem_core::ldpc::decoder::LdpcDecoder;
use modem_core::profile::LdpcRate;

fn rand_llr(seed: u32, n: usize, scale: f32) -> Vec<f32> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            (((s >> 16) as f32 / 32768.0) - 1.0) * scale
        })
        .collect()
}

fn main() {
    let rates = [
        ("R1_2", LdpcRate::R1_2),
        ("R2_3", LdpcRate::R2_3),
        ("R3_4", LdpcRate::R3_4),
        ("R5_6", LdpcRate::R5_6),
    ];
    let reps = 400usize;
    println!("LDPC decode_soft: scalar vs grouped vs SIMD (non-converging, 50 iters, {reps} reps)");
    println!("rate    scalar_us  grouped_us  simd_us   grp_x   simd_x");
    let time = |f: &dyn Fn() -> u8| -> f64 {
        let t = Instant::now();
        let mut acc = 0u64;
        for _ in 0..reps {
            acc = acc.wrapping_add(f() as u64);
        }
        std::hint::black_box(acc);
        t.elapsed().as_secs_f64() * 1e6 / reps as f64
    };
    for (name, rate) in rates {
        let dec = LdpcDecoder::new(rate, 50);
        let n = dec.n();
        // Pure-noise LLR (scale 0.3) → never satisfies syndrome → full 50 iters.
        let ch = rand_llr(0xBEEF, n, 0.3);

        let us0 = time(&|| dec.decode_soft(&ch, None, 0.7).0[0]);
        let us1 = time(&|| dec.decode_soft_grouped(&ch, None, 0.7).0[0]);
        let us2 = time(&|| dec.decode_soft_simd(&ch, None, 0.7).0[0]);

        println!(
            "{name:<6}  {us0:>8.1}  {us1:>9.1}  {us2:>7.1}  {:>5.2}x  {:>5.2}x",
            us0 / us1,
            us0 / us2,
        );
    }
}
