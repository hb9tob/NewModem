#!/usr/bin/env python3
"""
Simulateur de canal NBFM base sur les blocs GNU Radio (analog.nbfm_tx /
analog.nbfm_rx), avec generateur de bruit injecte a l'IF (entre
pre-emphase/modulation cote TX et demodulation/de-emphase cote RX).

Chaine :
  audio_in -> [HPF sub-audio 300 Hz pour emuler filtre CTCSS du transceiver]
           -> nbfm_tx  (LPF audio + preemphase + modulateur FM)
           -> + bruit gaussien complexe (IF)
           -> nbfm_rx  (demodulateur FM + de-emphase + LPF audio)
           -> audio_out

Parametres calibres sur les mesures reelles :
  - Deviation FM : 5000 Hz (defaut nbfm_tx)
  - Pre/de-emphase tau : 75 us
  - Audio rate : 48 kHz, IF rate : 480 kHz
  - HPF sub-audio : 300 Hz (coupure basse mesuree, due au filtre CTCSS)

Usage :
  python nbfm_channel_sim.py <input.wav> <output.wav> [--if-noise 0.02]
"""

import argparse
import math
import os
import numpy as np
import wave
from gnuradio import gr, blocks, analog, filter as gr_filter

AUDIO_RATE = 48000
IF_RATE = 480000
FM_DEV = 5000.0
TAU = 75e-6
SUB_AUDIO_HPF = 300.0   # filtre CTCSS du transceiver (Hz)
POST_LPF = 2000.0       # LPF audio du transceiver. Mesures OTA montrent
                        # degradation forte des que BW signal > 1500 Hz
                        # (Rs > 1200 Bd). Ancienne valeur 2400 Hz trop large.
POST_GAIN_DB = -6.5     # gain audio fixe (chaine acquisition reelle)
TX_HARD_CLIP = 0.55     # hard-clip audio avant FM (limiteur d'excursion).
                        # Calibre OTA : 32QAM 750 Bd a BER 5e-5 a -6 dB (peak
                        # 0.45) vs 7e-3 a 0 dB (peak 0.9) -> seuil ~0.5-0.6.
                        # 0 desactive le clip.

# Bruit audio post-demod (soundcard ADC, phase noise LO residuel).
# Mis a 0 : sim reste optimiste de 4-6 dB vs OTA a Rs > 1000 Bd mais la
# calibration flat-noise ne matche pas la forme freq-dependante du canal.
AUDIO_NOISE_RMS = 0.0

# Derive d'horloge soundcard (ppm). Modele : drift(t) = drift_ppm +
# drift_thermal_ppm * sin(2*pi*t/thermal_period_s)
DRIFT_PPM = -16.0               # derive statique (ppm), mesure reelle ~-16 ppm
DRIFT_THERMAL_PPM = 0.0         # amplitude de variation thermique (ppm)
DRIFT_THERMAL_PERIOD_S = 120.0  # periode de la variation (s)

# Delai aleatoire TX->RX pour forcer la synchro (evite alignement artificiel bit)
START_DELAY_MIN_S = 2.0
START_DELAY_MAX_S = 5.0


def load_wav(path):
    with wave.open(path, "r") as wf:
        nch = wf.getnchannels()
        sw = wf.getsampwidth()
        sr = wf.getframerate()
        n = wf.getnframes()
        raw = wf.readframes(n)
    if sw == 2:
        s = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
    else:
        raise ValueError(f"Format non supporte: {sw*8} bits")
    if nch == 2:
        s = s[::2]
    return s, sr


def write_wav(path, samples, sr):
    s = np.clip(samples * 32767.0, -32768, 32767).astype(np.int16)
    with wave.open(path, "w") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sr)
        wf.writeframes(s.tobytes())


def apply_clock_drift(audio, sr, drift_ppm=0.0, thermal_ppm=0.0,
                      thermal_period_s=120.0, thermal_phase=0.0,
                      thermal_shape="sine"):
    """
    Applique une derive d'horloge soundcard sur un signal audio.
    Modele : le clock effectif est sr * (1 + d(t)*1e-6) ou
      d(t) = drift_ppm + thermal_ppm * wave(2*pi*t/thermal_period_s + phase)

    `thermal_shape` :
      - "sine"     : oscillation lisse (defaut historique).
      - "triangle" : rampes lineaires avec inversions de pente — modele reel
        de la derive thermique (TX keyed -> temperature monte, la derive
        rampe ; ventilateurs -> elle redescend). Pente max = 4*amp/period,
        p.ex. 20 ppm/min avec amp=10 ppm sur period=120 s.

    Implementation : interpolation aux instants distordus par la derive
    cumulee (integrale du taux). Cela compresse/etire le temps de facon
    coherente avec un desaccord d'horloge TX vs RX.
    """
    if drift_ppm == 0.0 and thermal_ppm == 0.0:
        return audio
    n = len(audio)
    t = np.arange(n) / sr
    ph = 2 * np.pi * t / thermal_period_s + thermal_phase
    if thermal_shape == "triangle":
        # Triangle wave in [-1, 1], continuous, slope reverses at each apex.
        wave = (2.0 / np.pi) * np.arcsin(np.sin(ph))
    else:
        wave = np.sin(ph)
    # Taux d'ecart instantane (sans unite, ~1e-6)
    rate = drift_ppm * 1e-6 + thermal_ppm * 1e-6 * wave
    # Decalage temporel cumule (s)
    cum_shift = np.cumsum(rate) / sr
    # Instant d'echantillonnage dans le signal d'origine
    t_src = t - cum_shift
    t_src = np.clip(t_src, 0.0, (n - 1) / sr)
    # Cubic interpolation, NOT linear. Linear np.interp injects broadband
    # interpolation noise (~ -7 dB MER on the resampled passband) that destroys
    # high-order 64-APSK decode INDEPENDENTLY of the drift value — a simulator
    # artifact, not a real clock-drift effect. Cubic keeps the modem passband
    # (~300-2700 Hz, heavily oversampled at 48 kHz) clean so high-drift decode
    # is actually testable.
    from scipy.interpolate import CubicSpline
    return CubicSpline(t, audio)(t_src)


def simulate(audio_in, if_noise_voltage=0.0, sub_audio_hpf=SUB_AUDIO_HPF,
             post_lpf=POST_LPF, post_gain_db=POST_GAIN_DB,
             drift_ppm=DRIFT_PPM, thermal_ppm=DRIFT_THERMAL_PPM,
             thermal_period_s=DRIFT_THERMAL_PERIOD_S, thermal_shape="sine",
             tx_hard_clip=TX_HARD_CLIP,
             audio_noise_rms=AUDIO_NOISE_RMS,
             multipath_paths=None,
             carrier_gaps=None, noise_seed=0, if_chan_bw=0.0,
             start_delay_s=None, rng_seed=None, verbose=True):
    """Fait passer audio_in par TX -> bruit IF -> RX, retourne audio_out.

    Un delai initial (silence TX, mais porteuse + bruit RX actifs) est ajoute
    par defaut pour forcer la synchro du recepteur. Duree tiree au hasard dans
    [START_DELAY_MIN_S, START_DELAY_MAX_S] si start_delay_s est None.
    Passer start_delay_s=0 pour desactiver.

    `carrier_gaps`: list of (start_s, dur_s) in input-audio time (after the
    start delay is prepended, i.e. absolute time in the returned signal).
    During a gap the modulated IF carrier is multiplied by 0, so the
    discriminator only sees IF noise -- the open-squelch noise burst a
    receiver produces when the repeater carrier drops (fading, PTT bounce).
    """
    rng = np.random.RandomState(rng_seed)
    if start_delay_s is None:
        start_delay_s = float(rng.uniform(START_DELAY_MIN_S, START_DELAY_MAX_S))
    if start_delay_s > 0:
        n_pad = int(start_delay_s * AUDIO_RATE)
        audio_in = np.concatenate([np.zeros(n_pad, dtype=np.float32),
                                   audio_in.astype(np.float32)])
        if verbose:
            print(f"[sim] delai initial: {start_delay_s:.3f} s "
                  f"(porteuse + bruit IF seuls)")

    # Hard-clip TX (limiteur d'excursion du modulateur FM reel).
    # Applique avant la chaine GNU Radio car le clip instantane doit operer
    # sur l'amplitude audio, pas sur l'IF complexe.
    if tx_hard_clip and tx_hard_clip > 0:
        clip_count = int(np.sum(np.abs(audio_in) > tx_hard_clip))
        if clip_count > 0 and verbose:
            print(f"[sim] TX hard-clip a +/-{tx_hard_clip:.3f} : "
                  f"{clip_count} samples ({100*clip_count/len(audio_in):.2f}%)")
        audio_in = np.clip(audio_in, -tx_hard_clip, tx_hard_clip)

    tb = gr.top_block("nbfm_sim")

    src = blocks.vector_source_f(audio_in.astype(np.float32).tolist(), False)

    use_hpf = sub_audio_hpf and sub_audio_hpf > 0
    if use_hpf:
        taps = gr_filter.firdes.high_pass(
            1.0, AUDIO_RATE, sub_audio_hpf, 50.0)
        hpf = gr_filter.fir_filter_fff(1, taps)

    nbfm_tx = analog.nbfm_tx(
        audio_rate=AUDIO_RATE, quad_rate=IF_RATE,
        tau=TAU, max_dev=FM_DEV, fh=-1.0)
    nbfm_rx = analog.nbfm_rx(
        audio_rate=AUDIO_RATE, quad_rate=IF_RATE,
        tau=TAU, max_dev=FM_DEV)

    # Bruit complexe a l'IF (apres modulateur, avant demodulateur).
    # Represente le bruit thermique du recepteur.
    # noise_source_c avec amplitude=v -> Re et Im gaussiennes std v.
    noise_src = analog.noise_source_c(analog.GR_GAUSSIAN,
                                      float(if_noise_voltage), int(noise_seed))
    adder = blocks.add_cc()

    # Multipath IF-level : modelise la reception indirecte (echos de trajets
    # reflechis). Applique au signal complexe IF apres nbfm_tx.
    # multipath_paths : liste de (delay_s, amplitude_dB, phase_rad).
    # Premier element = trajet direct (typ. delay=0, amp=0dB, phase=0).
    # Autres elements = echos.
    mp_summer = None
    mp_delays = []
    mp_gains = []
    if multipath_paths and len(multipath_paths) >= 2:
        mp_summer = blocks.add_cc()
        for delay_s, amp_dB, phase_rad in multipath_paths:
            n_delay = int(round(delay_s * IF_RATE))
            # Complex gain = amp_linear * exp(j*phase)
            amp_lin = 10.0 ** (amp_dB / 20.0)
            complex_gain = amp_lin * (math.cos(phase_rad) + 1j * math.sin(phase_rad))
            mp_delays.append(blocks.delay(gr.sizeof_gr_complex, n_delay))
            mp_gains.append(blocks.multiply_const_cc(complex(complex_gain)))

    # Carrier gate (dropouts): multiplies the IF carrier by a 0/1 envelope.
    gate_mul = None
    if carrier_gaps:
        # Envelope built at the audio rate and sample-and-held up to the IF
        # rate (blocks.repeat): a 48 kHz-resolution edge is plenty, and a
        # python list of IF-rate complex samples would cost ~1 GB per minute.
        env = np.ones(len(audio_in), dtype=np.float32)
        for g_start, g_dur in carrier_gaps:
            a = max(0, int(round(g_start * AUDIO_RATE)))
            b = min(len(env), int(round((g_start + g_dur) * AUDIO_RATE)))
            env[a:b] = 0.0
            if verbose:
                print(f"[sim] carrier gap: {g_start:.3f} s .. "
                      f"{g_start + g_dur:.3f} s")
        gate_src_f = blocks.vector_source_f(env.tolist(), False)
        gate_rep = blocks.repeat(gr.sizeof_float, IF_RATE // AUDIO_RATE)
        gate_src = blocks.float_to_complex()
        gate_mul = blocks.multiply_cc()

    # Optional IF channel-select filter (RX selectivity). Off by default to
    # keep the historical calibration. Without it the discriminator sees the
    # full +/-240 kHz IF noise, which only matters when the carrier is absent
    # (carrier_gaps): the open-squelch noise then comes out ~20 dB hotter
    # than on a real receiver.
    chan_flt = None
    if if_chan_bw and if_chan_bw > 0:
        chan_taps = gr_filter.firdes.low_pass(
            1.0, IF_RATE, if_chan_bw, if_chan_bw * 0.25)
        chan_flt = gr_filter.fir_filter_ccf(1, chan_taps)

    # Post-demod audio LPF pour emuler le filtre audio du RX reel
    use_post_lpf = post_lpf and post_lpf > 0
    if use_post_lpf:
        lpf_taps = gr_filter.firdes.low_pass(
            1.0, AUDIO_RATE, post_lpf, 400.0)
        post_lpf_blk = gr_filter.fir_filter_fff(1, lpf_taps)

    # Gain audio fixe (calibration chaine)
    gain_lin = 10 ** (post_gain_db / 20.0)
    mul = blocks.multiply_const_ff(float(gain_lin))

    sink = blocks.vector_sink_f()

    if use_hpf:
        tb.connect(src, hpf, nbfm_tx)
    else:
        tb.connect(src, nbfm_tx)

    if gate_mul is not None:
        tb.connect(nbfm_tx, (gate_mul, 0))
        tb.connect(gate_src_f, gate_rep, gate_src, (gate_mul, 1))
        tx_out = gate_mul
    else:
        tx_out = nbfm_tx

    if mp_summer is not None:
        # tx_out -> N branches (delay + gain complexe) -> mp_summer -> adder
        for i, (delay_blk, gain_blk) in enumerate(zip(mp_delays, mp_gains)):
            tb.connect(tx_out, delay_blk, gain_blk, (mp_summer, i))
        tb.connect(mp_summer, (adder, 0))
    else:
        tb.connect(tx_out, (adder, 0))
    tb.connect(noise_src, (adder, 1))
    rx_in = adder
    if chan_flt is not None:
        tb.connect(adder, chan_flt)
        rx_in = chan_flt
    if use_post_lpf:
        tb.connect(rx_in, nbfm_rx, post_lpf_blk, mul, sink)
    else:
        tb.connect(rx_in, nbfm_rx, mul, sink)

    if verbose:
        print(f"[sim] audio_in: {len(audio_in)} samples ({len(audio_in)/AUDIO_RATE:.2f} s)")
        print(f"[sim] IF noise voltage: {if_noise_voltage}")
        print(f"[sim] sub-audio HPF: {sub_audio_hpf} Hz" if use_hpf else "[sim] no HPF")

    tb.run()
    audio_out = np.array(sink.data(), dtype=np.float64)

    # Bruit audio post-demod (soundcard ADC, phase noise residuel, etc).
    # Calibre sur mesures OTA : +EVM ~10 % vs sim pur.
    if audio_noise_rms and audio_noise_rms > 0:
        audio_out = audio_out + rng.normal(0.0, audio_noise_rms, len(audio_out))

    # Derive d'horloge (post-traitement)
    if drift_ppm != 0.0 or thermal_ppm != 0.0:
        if verbose:
            print(f"[sim] clock drift: static {drift_ppm:+.1f} ppm, "
                  f"thermal +/-{thermal_ppm:.1f} ppm / {thermal_period_s:.0f} s")
        audio_out = apply_clock_drift(
            audio_out, AUDIO_RATE,
            drift_ppm=drift_ppm, thermal_ppm=thermal_ppm,
            thermal_period_s=thermal_period_s, thermal_shape=thermal_shape)

    return audio_out


def load_prefix_wav(path):
    """Load a prefix recording (voice, ...) as mono float at AUDIO_RATE."""
    x, sr = load_wav(path)
    if sr != AUDIO_RATE:
        from math import gcd
        from scipy.signal import resample_poly
        g = gcd(AUDIO_RATE, sr)
        x = resample_poly(x, AUDIO_RATE // g, sr // g).astype(np.float32)
    return x


def simulate_link(burst, hops=1, prefix=None, prefix_gap_s=0.0,
                  truncate_head_s=0.0, gaps_rel=None, start_delay_s=None,
                  drift_ppm=DRIFT_PPM, rng_seed=None, verbose=True, **kw):
    """Repeater-aware wrapper around simulate().

    Layout of the transmitted audio (before the channel):
        [start delay: carrier + IF noise only][prefix][prefix gap][burst]
    with `truncate_head_s` cut from the head of `burst` first (repeater
    opening time eating the start of the transmission).

    `hops` > 1 replays the FM chain N times: a repeater is a two-hop
    cascade (user TX -> repeater RX, repeater TX -> user RX), each hop
    adding its own pre/de-emphasis, CTCSS HPF, audio LPF, limiter and IF
    noise. Intermediate hops are re-levelled to the burst's TX RMS so the
    repeater re-transmits at the same deviation. Clock drift is end-to-end
    (TX soundcard vs RX soundcard) and is applied once, after the last hop.

    `gaps_rel`: list of (start_s, dur_s) relative to the start of the
    (truncated) burst. Applied as carrier dropouts on the last hop
    (downlink fade -> open-squelch noise at the user's RX).

    Returns (audio_out, info) where info gives the nominal sample index of
    every landmark in the *undrifted* output timeline.
    """
    rng = np.random.RandomState(rng_seed)
    if start_delay_s is None:
        start_delay_s = float(rng.uniform(START_DELAY_MIN_S, START_DELAY_MAX_S))
    burst = np.asarray(burst, dtype=np.float32)
    n_trunc = int(round(truncate_head_s * AUDIO_RATE))
    if n_trunc > 0:
        burst = burst[n_trunc:]
        if verbose:
            print(f"[sim] truncate head: {truncate_head_s * 1000:.0f} ms "
                  f"({n_trunc} samples)")
    parts = []
    if prefix is not None and len(prefix) > 0:
        parts.append(np.asarray(prefix, dtype=np.float32))
        if prefix_gap_s > 0:
            parts.append(np.zeros(int(round(prefix_gap_s * AUDIO_RATE)),
                                  dtype=np.float32))
    n_pre = sum(len(p) for p in parts)
    audio_in = np.concatenate(parts + [burst]) if parts else burst
    t_burst = start_delay_s + n_pre / AUDIO_RATE
    gaps_abs = [(t_burst + a, d) for a, d in (gaps_rel or [])]

    burst_rms = float(np.sqrt(np.mean(burst.astype(np.float64) ** 2)))
    x = audio_in
    for h in range(hops):
        last = h == hops - 1
        x = simulate(x,
                     start_delay_s=start_delay_s if h == 0 else 0.0,
                     drift_ppm=0.0, thermal_ppm=0.0,
                     post_gain_db=kw.get("post_gain_db", POST_GAIN_DB) if last else 0.0,
                     carrier_gaps=gaps_abs if last else None,
                     noise_seed=(0 if rng_seed is None else rng_seed) * 16 + h,
                     rng_seed=None if rng_seed is None else rng_seed + h,
                     verbose=verbose,
                     **{k: v for k, v in kw.items()
                        if k not in ("post_gain_db", "thermal_ppm")})
        if not last:
            # Re-level on the burst region so hop h+1 sees the TX deviation.
            a = int(round(t_burst * AUDIO_RATE))
            seg = x[a:a + len(burst)]
            g = burst_rms / max(float(np.sqrt(np.mean(seg ** 2))), 1e-9)
            x = (x * g).astype(np.float32)
            if verbose:
                print(f"[sim] hop {h + 1}/{hops} done, re-level x{g:.3f}")

    thermal_ppm = kw.get("thermal_ppm", 0.0)
    if drift_ppm != 0.0 or thermal_ppm != 0.0:
        if verbose:
            print(f"[sim] clock drift (end-to-end): {drift_ppm:+.1f} ppm")
        x = apply_clock_drift(x, AUDIO_RATE, drift_ppm=drift_ppm,
                              thermal_ppm=thermal_ppm,
                              thermal_period_s=kw.get("thermal_period_s",
                                                      DRIFT_THERMAL_PERIOD_S),
                              thermal_shape=kw.get("thermal_shape", "sine"))

    info = {
        "start_delay_s": start_delay_s,
        "prefix_s": n_pre / AUDIO_RATE,
        "burst_start_s": t_burst,
        "burst_len_s": len(burst) / AUDIO_RATE,
        "truncate_head_s": n_trunc / AUDIO_RATE,
        "gaps_abs_s": gaps_abs,
        "hops": hops,
        "drift_ppm": drift_ppm,
    }
    return x, info


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("input_wav")
    ap.add_argument("output_wav")
    ap.add_argument("--if-noise", type=float, default=0.02,
                    help="Amplitude (std) du bruit gaussien complexe a l'IF")
    ap.add_argument("--hpf", type=float, default=SUB_AUDIO_HPF,
                    help="Frequence de coupure HPF sub-audio (Hz, 0=desactive)")
    ap.add_argument("--drift-ppm", type=float, default=DRIFT_PPM,
                    help="Derive d'horloge statique (ppm)")
    ap.add_argument("--thermal-ppm", type=float, default=DRIFT_THERMAL_PPM,
                    help="Amplitude variation thermique (ppm crete)")
    ap.add_argument("--thermal-shape", choices=["sine", "triangle"], default="sine",
                    help="Forme de la derive thermique (triangle = rampes reelles TX/ventilo)")
    ap.add_argument("--thermal-period", type=float, default=DRIFT_THERMAL_PERIOD_S,
                    help="Periode variation thermique (s)")
    ap.add_argument("--start-delay", type=float, default=None,
                    help=f"Delai initial TX->RX (s). Defaut: aleatoire "
                         f"[{START_DELAY_MIN_S},{START_DELAY_MAX_S}]. "
                         f"Passer 0 pour desactiver.")
    ap.add_argument("--seed", type=int, default=None,
                    help="Seed aleatoire (pour delai reproductible)")
    ap.add_argument("--tx-clip", type=float, default=TX_HARD_CLIP,
                    help=f"Hard-clip audio avant FM (defaut {TX_HARD_CLIP}, "
                         "0=desactive)")
    ap.add_argument("--audio-noise", type=float, default=AUDIO_NOISE_RMS,
                    help=f"RMS bruit audio post-demod (defaut {AUDIO_NOISE_RMS})")
    ap.add_argument("--post-lpf", type=float, default=POST_LPF,
                    help=f"Coupure LPF audio RX (Hz, defaut {POST_LPF}, 0=desactive)")
    ap.add_argument("--carrier-hz", type=float, default=0.0,
                    help="Carrier frequency offset on the output (Hz) — SSB/QO-100 LO offset test")
    ap.add_argument("--carrier-drift-hz", type=float, default=0.0,
                    help="Carrier frequency drift rate (Hz/s), linear ramp — classic-station LO/up-converter drift")
    ap.add_argument("--prefix-wav", default=None,
                    help="Recording (voice...) sent through the same FM chain "
                         "before the burst")
    ap.add_argument("--prefix-gain-db", type=float, default=0.0,
                    help="Gain applied to --prefix-wav (dB)")
    ap.add_argument("--prefix-gap", type=float, default=0.0,
                    help="Silence between the prefix and the burst (s)")
    ap.add_argument("--hops", type=int, default=1,
                    help="Number of FM hops (2 = repeater cascade)")
    ap.add_argument("--truncate-head", type=float, default=0.0,
                    help="Cut this many ms from the head of the input "
                         "(repeater opening time)")
    ap.add_argument("--gap", action="append", default=[],
                    metavar="START:DUR",
                    help="Carrier dropout START:DUR seconds, relative to the "
                         "burst start (repeatable)")
    ap.add_argument("--if-chan-bw", type=float, default=0.0,
                    help="IF channel-select LPF cutoff (Hz, one-sided, "
                         "0=off). ~8000 for the 5 kHz-deviation sim.")
    args = ap.parse_args()

    audio_in, sr = load_wav(args.input_wav)
    if sr != AUDIO_RATE:
        raise ValueError(f"Sample rate attendu {AUDIO_RATE}, recu {sr}")
    print(f"Entree: {args.input_wav}  ({len(audio_in)/sr:.2f} s)")

    prefix = None
    if args.prefix_wav:
        prefix = load_prefix_wav(args.prefix_wav) * 10 ** (args.prefix_gain_db / 20.0)
        print(f"Prefix: {args.prefix_wav} ({len(prefix)/AUDIO_RATE:.2f} s, "
              f"{args.prefix_gain_db:+.1f} dB)")
    gaps = []
    for g in args.gap:
        a, d = g.split(":")
        gaps.append((float(a), float(d)))

    audio_out, info = simulate_link(
        audio_in, hops=args.hops, prefix=prefix,
        prefix_gap_s=args.prefix_gap,
        truncate_head_s=args.truncate_head / 1000.0,
        gaps_rel=gaps, start_delay_s=args.start_delay,
        drift_ppm=args.drift_ppm, rng_seed=args.seed,
        if_noise_voltage=args.if_noise,
        sub_audio_hpf=args.hpf,
        post_lpf=args.post_lpf,
        thermal_ppm=args.thermal_ppm,
        thermal_period_s=args.thermal_period,
        thermal_shape=args.thermal_shape,
        tx_hard_clip=args.tx_clip,
        audio_noise_rms=args.audio_noise,
        if_chan_bw=args.if_chan_bw)
    print(f"Burst start (nominal, pre-drift): {info['burst_start_s']:.3f} s")

    # Optional carrier frequency offset + linear drift (SSB single-sideband
    # shift via the analytic signal), applied to the modem's input audio: this
    # is what the modem sees when a classic-station LO / up-converter drifts.
    if args.carrier_hz != 0.0 or args.carrier_drift_hz != 0.0:
        from scipy.signal import hilbert
        t = np.arange(len(audio_out)) / AUDIO_RATE
        analytic = hilbert(audio_out.astype(np.float64))
        phase = 2.0 * np.pi * (args.carrier_hz * t + 0.5 * args.carrier_drift_hz * t * t)
        audio_out = np.real(analytic * np.exp(1j * phase)).astype(np.float32)
        print(f"Carrier: {args.carrier_hz:+.1f} Hz + {args.carrier_drift_hz:+.2f} Hz/s drift")

    peak = np.max(np.abs(audio_out))
    print(f"Sortie: {len(audio_out)} samples, crete {peak:.4f} "
          f"({20*np.log10(peak+1e-12):.1f} dBFS)")

    write_wav(args.output_wav, audio_out, AUDIO_RATE)
    print(f"Ecrit: {args.output_wav}")


if __name__ == "__main__":
    main()
