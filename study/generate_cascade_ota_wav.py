"""
Genere un WAV OTA contenant les 5 profils de la cascade 16-APSK/8PSK/QPSK,
enchaines avec silences + marqueurs, pour test sur relais NBFM amateur.

Sortie :
  - results/apsk16_ftn/ota/cascade_ota.wav  (audio 48 kHz mono 16-bit)
  - results/apsk16_ftn/ota/cascade_ota.json (metadata : profils, timings,
    seed, nb symboles... -- necessaire pour l'analyzer)

Usage : /c/Users/tous/radioconda/python.exe study/generate_cascade_ota_wav.py \
          [--n-symbols N] [--silence-s S] [--output DIR]
"""

import argparse
import json
import os
import sys
import wave

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from modem_apsk16_ftn_bench import (
    MOD_QPSK, MOD_8PSK, MOD_16APSK, build_tx, AUDIO_RATE,
)


PROFILES = [
    dict(name="MEGA",   rs=1500.0, beta=0.20, tau=30/32, mod_name="16-APSK",
         seed=2001),
    dict(name="HIGH",   rs=1500.0, beta=0.20, tau=1.0,   mod_name="16-APSK",
         seed=2002),
    dict(name="NORMAL", rs=1500.0, beta=0.20, tau=1.0,   mod_name="8PSK",
         seed=2003),
    dict(name="ROBUST", rs=1000.0, beta=0.25, tau=1.0,   mod_name="QPSK",
         seed=2004),
    dict(name="ULTRA",  rs=500.0,  beta=0.25, tau=1.0,   mod_name="QPSK",
         seed=2005),
]

MOD_FACTORIES = {
    "16-APSK": MOD_16APSK,
    "8PSK":    MOD_8PSK,
    "QPSK":    MOD_QPSK,
}


def write_wav(path: str, samples: np.ndarray, sr: int = AUDIO_RATE):
    samples = np.clip(samples, -1.0, 1.0)
    s = (samples * 32767).astype(np.int16)
    with wave.open(path, "w") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sr)
        wf.writeframes(s.tobytes())


def marker_tone(freq_hz: float, duration_s: float, amp: float = 0.5) -> np.ndarray:
    """Tone sinusoidal pour identifier un profil (optionnel, court).

    Frequences differentes par profil pour identification visuelle au
    spectre : 300 Hz pour MEGA, 500 pour HIGH, 800 pour NORMAL,
    1200 pour ROBUST, 1800 pour ULTRA.
    """
    n = int(duration_s * AUDIO_RATE)
    t = np.arange(n) / AUDIO_RATE
    # Fade in/out 10 ms pour eviter clicks
    env = np.ones(n)
    fade = int(0.01 * AUDIO_RATE)
    if fade * 2 < n:
        env[:fade] = np.linspace(0, 1, fade)
        env[-fade:] = np.linspace(1, 0, fade)
    return (amp * env * np.sin(2 * np.pi * freq_hz * t)).astype(np.float32)


MARKER_FREQS = {
    "MEGA":   300.0,
    "HIGH":   500.0,
    "NORMAL": 800.0,
    "ROBUST": 1200.0,
    "ULTRA":  1800.0,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n-symbols", type=int, default=5000,
                    help="Nb symboles data par profil (defaut 5000)")
    ap.add_argument("--silence-s", type=float, default=2.0,
                    help="Silence entre profils en secondes (defaut 2.0)")
    ap.add_argument("--marker-s", type=float, default=0.5,
                    help="Duree du tone marqueur avant chaque profil (defaut 0.5)")
    ap.add_argument("--output-dir", default=None,
                    help="Dossier de sortie (defaut results/apsk16_ftn/ota/)")
    args = ap.parse_args()

    out_dir = args.output_dir or os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        "..", "results", "apsk16_ftn", "ota")
    os.makedirs(out_dir, exist_ok=True)

    audio_chunks = []
    t_cursor = 0.0
    metadata = {
        "sample_rate": AUDIO_RATE,
        "n_data_symbols": args.n_symbols,
        "silence_s": args.silence_s,
        "marker_s": args.marker_s,
        "profiles": [],
    }

    # Silence initial (permet au PTT/relais de s'ouvrir)
    init_silence = 3.0
    audio_chunks.append(np.zeros(int(init_silence * AUDIO_RATE), dtype=np.float32))
    t_cursor += init_silence

    for prof in PROFILES:
        name = prof["name"]
        mod = MOD_FACTORIES[prof["mod_name"]]()
        rng = np.random.default_rng(prof["seed"])
        tx = build_tx(prof["rs"], prof["beta"], prof["tau"], mod,
                      n_data_symbols=args.n_symbols, rng=rng)
        pb = tx["passband"].astype(np.float32)

        # Marqueur optionnel (tone a frequence unique)
        if args.marker_s > 0:
            marker = marker_tone(MARKER_FREQS[name], args.marker_s)
            audio_chunks.append(marker)
            t_marker_start = t_cursor
            t_cursor += len(marker) / AUDIO_RATE
        else:
            t_marker_start = None

        t_profile_start = t_cursor
        audio_chunks.append(pb)
        t_cursor += len(pb) / AUDIO_RATE
        t_profile_end = t_cursor

        # Silence entre profils
        audio_chunks.append(np.zeros(int(args.silence_s * AUDIO_RATE),
                                     dtype=np.float32))
        t_cursor += args.silence_s

        metadata["profiles"].append({
            "name": name,
            "rs": prof["rs"],
            "beta": prof["beta"],
            "tau": prof["tau"],
            "mod_name": prof["mod_name"],
            "seed": prof["seed"],
            "n_data_symbols": args.n_symbols,
            "marker_freq_hz": MARKER_FREQS[name] if args.marker_s > 0 else None,
            "marker_start_s": t_marker_start,
            "profile_start_s": t_profile_start,
            "profile_end_s": t_profile_end,
            "debit_uncoded_bps": int(prof["rs"] * mod.bits_per_sym / prof["tau"]),
        })

        print(f"[{name}] mod={mod.name} Rs={prof['rs']:.2f} beta={prof['beta']} "
              f"tau={prof['tau']:.4f}: {t_profile_end - t_profile_start:.2f} s, "
              f"{args.n_symbols} symboles, "
              f"{int(prof['rs'] * mod.bits_per_sym / prof['tau'])} bit/s uncoded")

    # Silence final
    final_silence = 2.0
    audio_chunks.append(np.zeros(int(final_silence * AUDIO_RATE), dtype=np.float32))
    t_cursor += final_silence

    audio = np.concatenate(audio_chunks)
    metadata["total_duration_s"] = float(len(audio) / AUDIO_RATE)

    wav_path = os.path.join(out_dir, "cascade_ota.wav")
    json_path = os.path.join(out_dir, "cascade_ota.json")

    write_wav(wav_path, audio)
    with open(json_path, "w", encoding="utf-8") as f:
        json.dump(metadata, f, indent=2)

    print(f"\nEcrit : {wav_path} ({len(audio) / AUDIO_RATE:.2f} s)")
    print(f"Metadata : {json_path}")
    print()
    print("Procedure OTA :")
    print("  1) Jouer le WAV cote TX (via soundcard + adaptateur audio-radio)")
    print("  2) Enregistrer cote RX (sortie audio du transceiver -> soundcard PC)")
    print("  3) Sauver la capture en 48 kHz mono 16-bit, ex: rx_recording.wav")
    print("  4) Analyser :")
    print("     /c/Users/tous/radioconda/python.exe study/analyse_cascade_ota_recording.py \\")
    print("       <rx_recording.wav>  [--metadata results/apsk16_ftn/ota/cascade_ota.json]")


if __name__ == "__main__":
    main()
