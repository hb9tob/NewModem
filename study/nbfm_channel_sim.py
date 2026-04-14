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
import os
import numpy as np
import wave
from gnuradio import gr, blocks, analog, filter as gr_filter

AUDIO_RATE = 48000
IF_RATE = 480000
FM_DEV = 5000.0
TAU = 75e-6
SUB_AUDIO_HPF = 300.0   # filtre CTCSS du transceiver (Hz)
POST_LPF = 2400.0       # LPF audio additionnel du transceiver (Hz), 0=desactive
POST_GAIN_DB = -6.5     # gain audio fixe (chaine acquisition reelle)
TX_HARD_CLIP = 0.55     # hard-clip audio avant FM (limiteur d'excursion).
                        # Calibre OTA : 32QAM 750 Bd a BER 5e-5 a -6 dB (peak
                        # 0.45) vs 7e-3 a 0 dB (peak 0.9) -> seuil ~0.5-0.6.
                        # 0 desactive le clip.

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
                      thermal_period_s=120.0, thermal_phase=0.0):
    """
    Applique une derive d'horloge soundcard sur un signal audio.
    Modele : le clock effectif est sr * (1 + d(t)*1e-6) ou
      d(t) = drift_ppm + thermal_ppm * sin(2*pi*t/thermal_period_s + phase)

    Implementation : interpolation aux instants distordus par la derive
    cumulee (integrale du taux). Cela compresse/etire le temps de facon
    coherente avec un desaccord d'horloge TX vs RX.
    """
    if drift_ppm == 0.0 and thermal_ppm == 0.0:
        return audio
    n = len(audio)
    t = np.arange(n) / sr
    # Taux d'ecart instantane (sans unite, ~1e-6)
    rate = drift_ppm * 1e-6 + thermal_ppm * 1e-6 * np.sin(
        2 * np.pi * t / thermal_period_s + thermal_phase)
    # Decalage temporel cumule (s)
    cum_shift = np.cumsum(rate) / sr
    # Instant d'echantillonnage dans le signal d'origine
    t_src = t - cum_shift
    t_src = np.clip(t_src, 0.0, (n - 1) / sr)
    return np.interp(t_src, t, audio)


def simulate(audio_in, if_noise_voltage=0.0, sub_audio_hpf=SUB_AUDIO_HPF,
             post_lpf=POST_LPF, post_gain_db=POST_GAIN_DB,
             drift_ppm=DRIFT_PPM, thermal_ppm=DRIFT_THERMAL_PPM,
             thermal_period_s=DRIFT_THERMAL_PERIOD_S,
             tx_hard_clip=TX_HARD_CLIP,
             start_delay_s=None, rng_seed=None, verbose=True):
    """Fait passer audio_in par TX -> bruit IF -> RX, retourne audio_out.

    Un delai initial (silence TX, mais porteuse + bruit RX actifs) est ajoute
    par defaut pour forcer la synchro du recepteur. Duree tiree au hasard dans
    [START_DELAY_MIN_S, START_DELAY_MAX_S] si start_delay_s est None.
    Passer start_delay_s=0 pour desactiver.
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
                                      float(if_noise_voltage), 0)
    adder = blocks.add_cc()

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
    tb.connect(nbfm_tx, (adder, 0))
    tb.connect(noise_src, (adder, 1))
    if use_post_lpf:
        tb.connect(adder, nbfm_rx, post_lpf_blk, mul, sink)
    else:
        tb.connect(adder, nbfm_rx, mul, sink)

    if verbose:
        print(f"[sim] audio_in: {len(audio_in)} samples ({len(audio_in)/AUDIO_RATE:.2f} s)")
        print(f"[sim] IF noise voltage: {if_noise_voltage}")
        print(f"[sim] sub-audio HPF: {sub_audio_hpf} Hz" if use_hpf else "[sim] no HPF")

    tb.run()
    audio_out = np.array(sink.data(), dtype=np.float64)

    # Derive d'horloge (post-traitement)
    if drift_ppm != 0.0 or thermal_ppm != 0.0:
        if verbose:
            print(f"[sim] clock drift: static {drift_ppm:+.1f} ppm, "
                  f"thermal +/-{thermal_ppm:.1f} ppm / {thermal_period_s:.0f} s")
        audio_out = apply_clock_drift(
            audio_out, AUDIO_RATE,
            drift_ppm=drift_ppm, thermal_ppm=thermal_ppm,
            thermal_period_s=thermal_period_s)

    return audio_out


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
    args = ap.parse_args()

    audio_in, sr = load_wav(args.input_wav)
    if sr != AUDIO_RATE:
        raise ValueError(f"Sample rate attendu {AUDIO_RATE}, recu {sr}")
    print(f"Entree: {args.input_wav}  ({len(audio_in)/sr:.2f} s)")

    audio_out = simulate(audio_in, if_noise_voltage=args.if_noise,
                         sub_audio_hpf=args.hpf,
                         drift_ppm=args.drift_ppm,
                         thermal_ppm=args.thermal_ppm,
                         thermal_period_s=args.thermal_period,
                         tx_hard_clip=args.tx_clip,
                         start_delay_s=args.start_delay,
                         rng_seed=args.seed)

    peak = np.max(np.abs(audio_out))
    print(f"Sortie: {len(audio_out)} samples, crete {peak:.4f} "
          f"({20*np.log10(peak+1e-12):.1f} dBFS)")

    write_wav(args.output_wav, audio_out, AUDIO_RATE)
    print(f"Ecrit: {args.output_wav}")


if __name__ == "__main__":
    main()
