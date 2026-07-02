//! Staged SFO (symbol-timing) recovery bench — FIRST LOOP ONLY.
//!
//! RESAMPLER -> RRC(matched filter) -> TED(Gardner/AbsGardner) -> seed & feed
//! the resampler. No FFE, no decode. Every signal is a WAV produced by the real
//! TX (`turbo_sim tx`, real embedded preambles) through the validated Python
//! channel simulator (`study/nbfm_channel_sim.py`, known --drift-ppm = truth).
//!
//! Stages (env `STAGE`):
//!   seed    — locate the 2 preambles on the post-RRC T/2 stream, report the
//!             coarse SFO seed (+ tau0) vs the known injected drift.
//!   kd      — measure the TED detector gain Kd from a +/-delta WAV pair and
//!             print the Rice Ch.8 Kp/Ki for a target loop time constant.
//!   close   — seed the loop from the 2-preamble estimate, close it, report
//!             convergence to the injected drift.
//!   thermal — same, with a slow sinusoidal drift; report tracking.
//!
//! Usage:
//!   STAGE=seed  CAL_WAV=cal.wav sfo_bench <impaired.wav> <PROFILE> [D_TRUE=..]
//!   STAGE=kd    sfo_bench <PROFILE> <wav_minus> <wav_plus> <delta_ppm>
//!   STAGE=close CAL_WAV=cal.wav KP=.. KI=.. [SEED_SIGN=1] [CTRL_SIGN=-1] sfo_bench <impaired.wav> <PROFILE>
//!   STAGE=thermal ... TH_OFFSET=.. TH_AMP=.. TH_PERIOD_S=.. sfo_bench <thermal.wav> <PROFILE>
//! Common env: TED=gardner|absgardner (default: auto by constellation),
//!   TAU_LOOP (s, default 0.1), ZETA (default 0.707), SEED_PPM (override seed).

use hound::WavReader;
use modem_core::frame::make_constellation;
use modem_core::preamble::make_preamble_for_config;
use modem_core::marker::make_sync_pattern;
use modem_core::profile::{ModemConfig, ProfileIndex};
use modem_core::sync::{estimate_sfo_seed_two_preambles, find_sync_landings, SyncLanding};
use modem_core::types::{Complex64, AUDIO_RATE};
use modem_core_base::streaming_dsp::StreamingDsp;
use modem_core_base::timing_loop::TedVariant;
use modem_core_base::timing_tracker::TimingTracker;

fn read_wav(path: &str) -> Vec<f32> {
    let mut r = WavReader::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
}

fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
}

/// Pick the TED variant the way the live receiver does: >1 distinct ring
/// magnitude ⇒ AbsGardner (APSK), else Gardner (QPSK/8PSK). `TED=` overrides.
fn auto_ted(cfg: &ModemConfig) -> TedVariant {
    if let Ok(v) = std::env::var("TED") {
        return match v.as_str() {
            "absgardner" | "abs" => TedVariant::AbsGardner,
            _ => TedVariant::Gardner,
        };
    }
    let cons = make_constellation(cfg);
    let mut mags: Vec<f64> = cons.points.iter().map(|p| p.norm()).collect();
    mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    mags.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    if mags.len() > 1 { TedVariant::AbsGardner } else { TedVariant::Gardner }
}

/// Run the DSP open-loop over the whole WAV at a fixed seed rate and return the
/// full accumulated post-RRC T/2 stream (fse) plus its absolute start index.
fn accumulate_fse(cfg: &ModemConfig, audio: &[f32], seed_ppm: f64) -> (Vec<Complex64>, u64) {
    let mut dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
    dsp.timing_enable(true);
    dsp.timing_seed(1.0 + seed_ppm * 1e-6, 0.0);
    let chunk = AUDIO_RATE as usize / 50; // 20 ms
    let mut full: Vec<Complex64> = Vec::new();
    let mut start_abs: Option<u64> = None;
    let mut end = 0usize;
    while end < audio.len() {
        end = (end + chunk).min(audio.len());
        dsp.feed_audio(&audio[..end], 0, 0.0);
        let s = dsp.sym_buffer_start_abs();
        let block = dsp.drain_symbols();
        if !block.is_empty() {
            if start_abs.is_none() {
                start_abs = Some(s);
            }
            full.extend_from_slice(&block);
        }
    }
    (full, start_abs.unwrap_or(0))
}

fn main() {
    let stage = std::env::var("STAGE").unwrap_or_else(|_| "seed".into());
    let args: Vec<String> = std::env::args().skip(1).collect();

    if stage == "kd" {
        stage_kd(&args);
        return;
    }
    if stage == "posnoise" {
        stage_posnoise(&args);
        return;
    }
    if stage == "track" {
        stage_track(&args);
        return;
    }

    // seed / close / thermal all take: <impaired.wav> <PROFILE>
    if args.len() < 2 {
        eprintln!("usage: STAGE={stage} sfo_bench <wav> <PROFILE>");
        std::process::exit(2);
    }
    let wav_path = &args[0];
    let cfg = ProfileIndex::from_name(&args[1])
        .unwrap_or_else(|| panic!("unknown profile {}", args[1]))
        .to_config();
    let ted = auto_ted(&cfg);
    let pre = make_preamble_for_config(&cfg);
    let seed_sign = if env_f64("SEED_SIGN", 1.0) < 0.0 { -1.0 } else { 1.0 };

    // Self-calibrate the nominal preamble spacing from a zero-drift reference.
    let nominal = match std::env::var("CAL_WAV") {
        Ok(cal) => {
            let (fse, s0) = accumulate_fse(&cfg, &read_wav(&cal), 0.0);
            let seed = estimate_sfo_seed_two_preambles(&fse, s0, /*pitch*/ 2, &pre, 0.0, 1.0)
                .expect("CAL_WAV: two preambles located for nominal self-calibration");
            eprintln!("[cal] nominal_spacing_syms={:.3} metric={:.3}", seed.spacing_syms, seed.metric);
            seed.spacing_syms
        }
        Err(_) => 0.0,
    };

    let audio = read_wav(wav_path);
    let (fse, s0) = accumulate_fse(&cfg, &audio, 0.0);
    let pitch_fse = {
        // pitch_fse is fixed by the profile geometry; derive via a throwaway DSP.
        let dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
        dsp.pitch_fse()
    };
    let seed = estimate_sfo_seed_two_preambles(&fse, s0, pitch_fse, &pre, nominal, seed_sign);

    match stage.as_str() {
        "seed" => {
            let d_true = std::env::var("D_TRUE").ok().and_then(|s| s.parse::<f64>().ok());
            match seed {
                Some(s) => {
                    let err = d_true.map(|d| s.slope_ppm - d);
                    println!(
                        "{:8} ted={:?} d_true={:>7} est_ppm={:+8.2} err_ppm={:>8} tau0={:+.4} spacing={:.2} metric={:.3}",
                        args[1], ted,
                        d_true.map(|d| format!("{d:+.0}")).unwrap_or_else(|| "?".into()),
                        s.slope_ppm,
                        err.map(|e| format!("{e:+.2}")).unwrap_or_else(|| "?".into()),
                        s.tau0_frac, s.spacing_syms, s.metric,
                    );
                }
                None => println!("{:8} ted={:?} SEED FAIL (preambles not located / low metric)", args[1], ted),
            }
        }
        "close" | "thermal" => {
            let seed = seed.unwrap_or_else(|| {
                // fall back to a manual seed if the estimator failed
                modem_core::sync::SfoSeed {
                    slope_ppm: env_f64("SEED_PPM", 0.0),
                    tau0_frac: 0.0, spacing_syms: 0.0, metric: 0.0,
                }
            });
            let seed_ppm = if std::env::var("SEED_PPM").is_ok() {
                env_f64("SEED_PPM", seed.slope_ppm)
            } else {
                seed.slope_ppm
            };
            run_closed(&cfg, &audio, ted, pitch_fse, seed_ppm, &stage);
        }
        other => eprintln!("unknown STAGE={other}"),
    }
}

/// Close the loop: seed the resampler + tracker at `seed_ppm`, feed forward,
/// slew the resampler each chunk, and log the loop's estimate vs the ground
/// truth (static `D_TRUE`, or the mirrored thermal sinusoid).
fn run_closed(cfg: &ModemConfig, audio: &[f32], ted: TedVariant, pitch_fse: usize, seed_ppm: f64, stage: &str) {
    let kp = env_f64("KP", 2e-5);
    let ki = env_f64("KI", 5e-7);
    let ctrl_sign = if env_f64("CTRL_SIGN", -1.0) < 0.0 { -1.0 } else { 1.0 };

    let mut dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
    dsp.timing_enable(true);
    dsp.timing_seed(1.0 + seed_ppm * 1e-6, 0.0); // rate seeded; τ0 → later (rewind)
    let mut tr = TimingTracker::new(ted, kp, ki, pitch_fse);
    tr.set_control_sign(ctrl_sign);
    tr.seed(seed_ppm);

    // Ground-truth curve.
    let d_true = env_f64("D_TRUE", f64::NAN);
    let th_off = env_f64("TH_OFFSET", 0.0);
    let th_amp = env_f64("TH_AMP", 0.0);
    let th_per = env_f64("TH_PERIOD_S", 1.0);
    let inj = |t: f64| -> f64 {
        if stage == "thermal" {
            th_off + th_amp * (2.0 * std::f64::consts::PI * t / th_per).sin()
        } else {
            d_true
        }
    };

    eprintln!(
        "[close] profile ted={ted:?} seed_ppm={seed_ppm:+.2} Kp={kp:.2e} Ki={ki:.2e} ctrl_sign={ctrl_sign:+.0}"
    );
    println!("# t_s inj_ppm rate_ppm resid_ppm ted");
    let chunk = AUDIO_RATE as usize / 50; // 20 ms
    let mut end = 0usize;
    let mut next_log = 0.0_f64;
    let mut last_rate = seed_ppm;
    while end < audio.len() {
        end = (end + chunk).min(audio.len());
        dsp.feed_audio(&audio[..end], 0, 0.0);
        let s = dsp.sym_buffer_start_abs();
        let block = dsp.drain_symbols();
        if !block.is_empty() {
            tr.feed(s, &block);
            dsp.set_resample_step(tr.rate());
            last_rate = tr.rate_ppm();
        }
        let t = end as f64 / AUDIO_RATE as f64;
        if t >= next_log {
            let inj_v = inj(t);
            println!(
                "{:.2} {:+.2} {:+.3} {:+.3} {:+.4}",
                t, inj_v, tr.rate_ppm(), tr.resid_ppm(), tr.last_err()
            );
            next_log += 0.5;
        }
    }
    // Steady-state summary over the last 25% of the run.
    let final_t = audio.len() as f64 / AUDIO_RATE as f64;
    eprintln!(
        "[close] final t={final_t:.1}s rate_ppm={last_rate:+.2} inj={:+.2} |err|={:.2}",
        inj(final_t), (last_rate - inj(final_t)).abs()
    );
}

/// STAGE=kd: measure the TED detector gain Kd from a +/-delta WAV pair, then
/// print the Rice Ch.8 loop constants for the target time constant.
fn stage_kd(args: &[String]) {
    if args.len() < 4 {
        eprintln!("usage: STAGE=kd sfo_bench <PROFILE> <wav_minus> <wav_plus> <delta_ppm>");
        std::process::exit(2);
    }
    let cfg = ProfileIndex::from_name(&args[0])
        .unwrap_or_else(|| panic!("unknown profile {}", args[0]))
        .to_config();
    let ted = auto_ted(&cfg);
    let delta: f64 = args[3].parse().expect("delta_ppm");

    let slope = |wav: &str| -> f64 {
        let audio = read_wav(wav);
        let pitch_fse = {
            let dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
            dsp.pitch_fse()
        };
        let mut dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
        dsp.timing_enable(true);
        dsp.timing_seed(1.0, 0.0);
        let mut tr = TimingTracker::new(ted, 0.0, 0.0, pitch_fse);
        tr.set_open_loop(true);
        tr.seed(0.0);
        // Cap the window so the timing phase stays inside the TED's unambiguous
        // ±0.5-symbol range (τ = D·1e-6·N): past that the S-curve wraps and the
        // slope is meaningless. Mirrors gardner_check's ~1500-symbol window.
        let n_cap: u64 = std::env::var("KD_NSYM").ok().and_then(|s| s.parse().ok()).unwrap_or(1500);
        let chunk = AUDIO_RATE as usize / 50;
        let mut end = 0usize;
        while end < audio.len() && tr.processed_syms() < n_cap {
            end = (end + chunk).min(audio.len());
            dsp.feed_audio(&audio[..end], 0, 0.0);
            let s = dsp.sym_buffer_start_abs();
            let block = dsp.drain_symbols();
            tr.feed(s, &block);
        }
        tr.ted_slope_per_sym()
    };

    let sm = slope(&args[1]);
    let sp = slope(&args[2]);
    // slope(D) ≈ Kd·D·1e-6  →  Kd = differential / (2·δ·1e-6). The Rice formula
    // wants the loop-gain MAGNITUDE (the S-curve polarity is set by control_sign).
    let kd_signed = (sp - sm) / (2.0 * delta * 1e-6);
    let kd = kd_signed.abs();

    // Rice, Digital Communications: A Discrete-Time Approach, Ch. 8.
    let pitch_fse = {
        let dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
        dsp.pitch_fse()
    } as f64;
    let k0 = 1.0 / pitch_fse;
    let tau_loop = env_f64("TAU_LOOP", 0.1);
    let zeta = env_f64("ZETA", 0.707);
    let bn = 1.0 / (2.0 * std::f64::consts::PI * tau_loop); // Hz
    let bnt = bn / cfg.symbol_rate;
    let theta = bnt / (zeta + 1.0 / (4.0 * zeta));
    let den = 1.0 + 2.0 * zeta * theta + theta * theta;
    let kp = (1.0 / (kd * k0)) * 4.0 * zeta * theta / den;
    let ki = (1.0 / (kd * k0)) * 4.0 * theta * theta / den;

    println!("# profile ted Rs slope_minus slope_plus Kd K0 BnT Kp Ki");
    println!(
        "{:8} {:?} {:.0} {:+.4e} {:+.4e} Kd={:+.4e} K0={:.3} BnT={:.3e} Kp={:.4e} Ki={:.4e}",
        args[0], ted, cfg.symbol_rate, sm, sp, kd_signed, k0, bnt, kp, ki
    );
}

/// Feedforward SFO(t) from the preamble landings — the data-aided timing tracker
/// (preamble = known constant-modulus reference, so modulation-independent, no
/// TED/Kd/bias). Locates every preamble, unwraps its fractional timing phase φ,
/// and slides a short LS window along the anchors: the local dφ/dsymbol IS the
/// SFO at that time. Logs SFO_est(t) against the injected truth (static D_TRUE or
/// the thermal sinusoid). Open-loop measurement — closing the loop is just
/// feeding SFO_est back to `set_resample_step` (already proven byte-exact).
fn stage_track(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: STAGE=track sfo_bench <wav> <PROFILE>");
        std::process::exit(2);
    }
    let cfg = ProfileIndex::from_name(&args[1])
        .unwrap_or_else(|| panic!("unknown profile {}", args[1]))
        .to_config();
    let pitch_fse = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz)
        .pitch_fse();
    if std::env::var("CLOSE").is_ok() {
        run_track_closed(&cfg, &read_wav(&args[0]), pitch_fse, &args[1]);
        return;
    }
    let (fse, s0) = accumulate_fse(&cfg, &read_wav(&args[0]), 0.0);
    let preamble = modem_core::preamble::make_preamble_for_config(&cfg);
    let pre = find_sync_landings(&fse, s0, pitch_fse, &preamble, 0.85);
    if pre.len() < 3 {
        eprintln!("track: only {} preambles — need ≥3", pre.len());
        return;
    }

    // Unwrap φ (symbols) along symbol-position.
    let xs: Vec<f64> = pre.iter().map(|l| l.pos_fse / pitch_fse as f64).collect();
    let ts: Vec<f64> = xs.iter().map(|x| x / cfg.symbol_rate).collect();
    let mut phi = Vec::with_capacity(pre.len());
    let mut acc = pre[0].phi;
    phi.push(acc);
    for w in pre.windows(2) {
        let mut d = w[1].phi - w[0].phi;
        while d > 0.5 {
            d -= 1.0;
        }
        while d < -0.5 {
            d += 1.0;
        }
        acc += d;
        phi.push(acc);
    }

    // Ground-truth curve (mirror the channel-sim args).
    let d_true = env_f64("D_TRUE", f64::NAN);
    let th_off = env_f64("TH_OFFSET", 0.0);
    let th_amp = env_f64("TH_AMP", 0.0);
    let th_per = env_f64("TH_PERIOD_S", 1.0);
    let thermal = std::env::var("TH_AMP").is_ok();
    let triangle = std::env::var("TH_SHAPE").map(|s| s == "triangle").unwrap_or(false);
    let inj = |t: f64| -> f64 {
        if thermal {
            let ph = 2.0 * std::f64::consts::PI * t / th_per;
            let wave = if triangle {
                (2.0 / std::f64::consts::PI) * ph.sin().asin()
            } else {
                ph.sin()
            };
            th_off + th_amp * wave
        } else {
            d_true
        }
    };

    // Sliding LS window over W preambles → local slope dφ/dsym = SFO.
    let w = env_f64("WIN", 4.0).max(2.0) as usize;
    println!("# t_s inj_ppm sfo_est_ppm err_ppm");
    let mut sum_abs = 0.0;
    let mut cnt = 0;
    for i in 0..pre.len() {
        let lo = i.saturating_sub(w / 2);
        let hi = (lo + w).min(pre.len());
        if hi - lo < 2 {
            continue;
        }
        let n = (hi - lo) as f64;
        let mx = xs[lo..hi].iter().sum::<f64>() / n;
        let my = phi[lo..hi].iter().sum::<f64>() / n;
        let mut sxx = 0.0;
        let mut sxy = 0.0;
        for j in lo..hi {
            sxx += (xs[j] - mx) * (xs[j] - mx);
            sxy += (xs[j] - mx) * (phi[j] - my);
        }
        let sfo = if sxx > 0.0 { sxy / sxx * 1e6 } else { 0.0 };
        let inj_v = inj(ts[i]);
        let err = sfo - inj_v;
        if inj_v.is_finite() {
            sum_abs += err.abs();
            cnt += 1;
        }
        println!("{:.2} {:+.2} {:+.3} {:+.3}", ts[i], inj_v, sfo, err);
    }
    if cnt > 0 {
        eprintln!(
            "[track] {} n_preamble={} WIN={} mean|err|={:.3}ppm",
            args[1], pre.len(), w, sum_abs / cnt as f64
        );
    }
}

/// Closed-loop preamble feedforward: process the WAV causally, and each time a
/// new preamble lands, take the two-point φ slope since the previous preamble
/// (the residual SFO over that gap) and integrate it into the resampler rate.
/// Coasts on the held rate between preambles. Logs rate vs the injected truth.
fn run_track_closed(cfg: &ModemConfig, audio: &[f32], pitch_fse: usize, prof: &str) {
    let gain = env_f64("GAIN", 0.5);
    let seed_ppm = env_f64("SEED_PPM", 0.0);
    let preamble = modem_core::preamble::make_preamble_for_config(cfg);

    // Ground-truth curve (mirror the channel-sim args).
    let d_true = env_f64("D_TRUE", f64::NAN);
    let th_off = env_f64("TH_OFFSET", 0.0);
    let th_amp = env_f64("TH_AMP", 0.0);
    let th_per = env_f64("TH_PERIOD_S", 1.0);
    let thermal = std::env::var("TH_AMP").is_ok();
    let triangle = std::env::var("TH_SHAPE").map(|s| s == "triangle").unwrap_or(false);
    let inj = |t: f64| -> f64 {
        if thermal {
            let ph = 2.0 * std::f64::consts::PI * t / th_per;
            let wave = if triangle {
                (2.0 / std::f64::consts::PI) * ph.sin().asin()
            } else {
                ph.sin()
            };
            th_off + th_amp * wave
        } else {
            d_true
        }
    };

    let mut dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
    dsp.timing_enable(true);
    let mut r_ppm = seed_ppm;
    dsp.timing_seed(1.0 + r_ppm * 1e-6, 0.0);

    let chunk = AUDIO_RATE as usize / 50; // 20 ms
    let mut full: Vec<Complex64> = Vec::new();
    let mut s0: Option<u64> = None;
    let mut end = 0usize;
    let mut chunk_i = 0usize;
    let mut prev: Option<(f64, f64)> = None; // (symbol position, φ wrapped)
    let mut last_sym = f64::NEG_INFINITY;
    let mut last_resid = 0.0;
    let mut next_log = 0.0;
    eprintln!("[close] {prof} seed_ppm={seed_ppm:+.2} GAIN={gain}");
    println!("# t_s inj_ppm rate_ppm resid_ppm");
    while end < audio.len() {
        end = (end + chunk).min(audio.len());
        dsp.feed_audio(&audio[..end], 0, 0.0);
        let s = dsp.sym_buffer_start_abs();
        let blk = dsp.drain_symbols();
        if !blk.is_empty() {
            if s0.is_none() {
                s0 = Some(s);
            }
            full.extend_from_slice(&blk);
        }
        chunk_i += 1;
        // Scan every ~0.5 s (preambles are ~4 s apart — fine granularity).
        if chunk_i % 25 == 0 {
            if let Some(start_abs) = s0 {
                let landings = find_sync_landings(&full, start_abs, pitch_fse, &preamble, 0.85);
                for l in &landings {
                    let sym = l.pos_fse / pitch_fse as f64;
                    if sym <= last_sym + 1000.0 {
                        continue; // already handled this preamble (gap-based dedup)
                    }
                    if let Some((psym, pphi)) = prev {
                        let mut dphi = l.phi - pphi;
                        while dphi > 0.5 {
                            dphi -= 1.0;
                        }
                        while dphi < -0.5 {
                            dphi += 1.0;
                        }
                        let dsym = sym - psym;
                        if dsym > 1.0 {
                            last_resid = dphi / dsym * 1e6; // residual SFO over the gap
                            r_ppm += gain * last_resid;
                            dsp.set_resample_step(1.0 + r_ppm * 1e-6);
                        }
                    }
                    prev = Some((sym, l.phi));
                    last_sym = sym;
                }
            }
        }
        let t = end as f64 / AUDIO_RATE as f64;
        if t >= next_log {
            println!("{:.2} {:+.2} {:+.3} {:+.3}", t, inj(t), r_ppm, last_resid);
            next_log += 2.0;
        }
    }
    let final_t = audio.len() as f64 / AUDIO_RATE as f64;
    eprintln!(
        "[close] {prof} final rate_ppm={r_ppm:+.2} inj={:+.2} |err|={:.2}",
        inj(final_t),
        (r_ppm - inj(final_t)).abs()
    );
}

/// Per-anchor timing-phase noise of the reference symbols — how much a single
/// preamble / marker-sync landing jitters, and hence how much averaging the
/// feedforward loop needs. Unwraps φ, removes the linear drift (the true SFO
/// ramp), and reports the residual std in symbols. Run across an `--if-noise`
/// sweep to see the noise-vs-SNR curve for each reference type.
fn stage_posnoise(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: STAGE=posnoise sfo_bench <wav> <PROFILE>");
        std::process::exit(2);
    }
    let cfg = ProfileIndex::from_name(&args[1])
        .unwrap_or_else(|| panic!("unknown profile {}", args[1]))
        .to_config();
    let pitch_fse = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz)
        .pitch_fse();
    let (fse, s0) = accumulate_fse(&cfg, &read_wav(&args[0]), 0.0);

    let preamble = modem_core::preamble::make_preamble_for_config(&cfg);
    let sync = make_sync_pattern();

    // Detrend φ vs symbol-position; residual std = per-anchor timing-phase noise.
    // Returns (count, resid_std_syms, drift_ppm, mean_metric).
    let analyse = |landings: &[SyncLanding]| -> (usize, f64, f64, f64) {
        let n = landings.len();
        if n < 3 {
            return (n, f64::NAN, f64::NAN, f64::NAN);
        }
        // Unwrap φ along position (true per-step change ≪ 0.5 at these drifts).
        let mut xs = Vec::with_capacity(n); // symbol position
        let mut ys = Vec::with_capacity(n); // unwrapped φ (symbols)
        let mut acc = landings[0].phi;
        xs.push(landings[0].pos_fse / pitch_fse as f64);
        ys.push(acc);
        for w in landings.windows(2) {
            let mut d = w[1].phi - w[0].phi;
            while d > 0.5 {
                d -= 1.0;
            }
            while d < -0.5 {
                d += 1.0;
            }
            acc += d;
            xs.push(w[1].pos_fse / pitch_fse as f64);
            ys.push(acc);
        }
        // LS line y = a + b·x.
        let nf = n as f64;
        let mx = xs.iter().sum::<f64>() / nf;
        let my = ys.iter().sum::<f64>() / nf;
        let mut sxx = 0.0;
        let mut sxy = 0.0;
        for i in 0..n {
            sxx += (xs[i] - mx) * (xs[i] - mx);
            sxy += (xs[i] - mx) * (ys[i] - my);
        }
        let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        let a = my - b * mx;
        let mut ss = 0.0;
        for i in 0..n {
            let r = ys[i] - (a + b * xs[i]);
            ss += r * r;
        }
        let resid_std = (ss / (nf - 2.0)).sqrt();
        let mean_metric = landings.iter().map(|l| l.metric).sum::<f64>() / nf;
        (n, resid_std, b * 1e6, mean_metric)
    };

    let mk_gate = env_f64("MK_GATE", 0.2);
    let pre = find_sync_landings(&fse, s0, pitch_fse, &preamble, 0.2);
    let mk = find_sync_landings(&fse, s0, pitch_fse, &sync, mk_gate);
    let (pn, ps, pd, pm) = analyse(&pre);
    let (mn, ms, md, mm) = analyse(&mk);
    println!(
        "{:8} preamble n={:2} noise={:.4}sym drift={:+7.2}ppm metric={:.3} | marker n={:3} noise={:.4}sym drift={:+7.2}ppm metric={:.3}",
        args[1], pn, ps, pd, pm, mn, ms, md, mm
    );
}
