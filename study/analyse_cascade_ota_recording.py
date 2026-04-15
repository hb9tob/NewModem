"""
Analyse une capture OTA (WAV enregistre cote RX) pour decoder chaque profil
de la cascade 16-APSK/8PSK/QPSK et comparer les metriques a la reference.

Principe :
  1) Charger la capture RX (WAV 48 kHz mono) et la metadata cascade_ota.json.
  2) Pour chaque profil : localiser le segment dans la capture (par correlation
     du preambule ou par marqueur tone), regenerer le tx_info deterministe
     (meme seed), decoder via decode_rx_audio, reporter BER/GMI/EVM/SNR.
  3) Sortie : JSON + CSV + PNG constellation par profil.

Usage : /c/Users/tous/radioconda/python.exe study/analyse_cascade_ota_recording.py \
          rx_recording.wav [--metadata cascade_ota.json]
"""

import argparse
import json
import os
import sys
import wave

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from modem_apsk16_ftn_bench import (
    MOD_QPSK, MOD_8PSK, MOD_16APSK,
    build_tx, decode_rx_audio, AUDIO_RATE,
    plot_constellation, rx_matched_and_timing,
)


MOD_FACTORIES = {
    "16-APSK": MOD_16APSK,
    "8PSK":    MOD_8PSK,
    "QPSK":    MOD_QPSK,
}


def load_wav(path: str):
    with wave.open(path, "r") as wf:
        nch = wf.getnchannels()
        sw = wf.getsampwidth()
        sr = wf.getframerate()
        n = wf.getnframes()
        raw = wf.readframes(n)
    if sw == 2:
        s = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    elif sw == 3:
        s = np.frombuffer(raw, dtype=np.uint8).reshape(-1, 3)
        s = (s[:, 0].astype(np.int32) |
             (s[:, 1].astype(np.int32) << 8) |
             (s[:, 2].astype(np.int32) << 16))
        s = np.where(s & 0x800000, s - 0x1000000, s).astype(np.float64) / 8388608.0
    elif sw == 4:
        s = np.frombuffer(raw, dtype=np.int32).astype(np.float64) / 2147483648.0
    else:
        raise ValueError(f"Format non supporte: {sw*8} bits")
    if nch == 2:
        s = s.reshape(-1, 2).mean(axis=1)
    return s, sr


def locate_profile_by_preamble(rx_audio, tx_info, search_window_s,
                               coarse_start_s=0.0):
    """Trouve la position du preambule dans rx_audio par correlation.

    Retourne l'index de debut en samples (position ou commence la portion
    TX complete, y compris le ramp RRC avant le 1er symbole).
    """
    # Extrait un segment dans la fenetre de recherche autour de coarse_start
    i0 = max(0, int((coarse_start_s - 1.0) * AUDIO_RATE))
    i1 = min(len(rx_audio),
             int((coarse_start_s + search_window_s + 1.0) * AUDIO_RATE))
    segment = rx_audio[i0:i1]

    # Utilise le pipeline rx_matched_and_timing du banc sur CE segment :
    # il fait downmix + matched filter + correlation preambule -> sync_pos.
    rx_info = rx_matched_and_timing(segment, tx_info)
    # sync_pos_48k est relatif au segment ; absolu = i0 + sync_pos_48k
    # (c'est la position du 1er pic symbole du preambule)
    # Pour decoder, on passe le segment DIRECT a decode_rx_audio plus tard.
    return rx_info, i0, segment


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("recording", help="WAV RX enregistre 48 kHz mono")
    ap.add_argument("--metadata", default=None,
                    help="JSON metadata (defaut : cascade_ota.json dans le "
                         "meme dossier que la capture)")
    ap.add_argument("--output-dir", default=None,
                    help="Dossier sortie (defaut : a cote de la capture)")
    args = ap.parse_args()

    rec_dir = os.path.dirname(os.path.abspath(args.recording))
    meta_path = args.metadata or os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        "..", "results", "apsk16_ftn", "ota", "cascade_ota.json")
    out_dir = args.output_dir or rec_dir
    os.makedirs(out_dir, exist_ok=True)

    # Charge
    rx_audio, sr = load_wav(args.recording)
    print(f"RX : {len(rx_audio)} samples @ {sr} Hz "
          f"({len(rx_audio)/sr:.2f} s)")
    if sr != AUDIO_RATE:
        print(f"!! Sample rate {sr} != {AUDIO_RATE}. Resample recommande.")
        # Basic resample
        try:
            from scipy.signal import resample_poly
            from math import gcd
            g = gcd(sr, AUDIO_RATE)
            up, down = AUDIO_RATE // g, sr // g
            rx_audio = resample_poly(rx_audio, up, down)
            print(f"   Resample applique : {up}/{down}")
        except Exception as e:
            print(f"   Resample echec : {e}. Continue (risque).")

    with open(meta_path, encoding="utf-8") as f:
        meta = json.load(f)

    # Pour chaque profil : regenerate tx_info, decoder, reporter
    results = []
    csv_path = os.path.join(out_dir, "ota_cascade_results.csv")
    with open(csv_path, "w", encoding="utf-8") as f:
        f.write("profile,rs,beta,tau,mod,ber,ser,gmi,gmi_max,evm,snr_db,"
                "n_symbols_decoded\n")

    for prof_meta in meta["profiles"]:
        name = prof_meta["name"]
        print(f"\n=== {name} ({prof_meta['mod_name']} "
              f"Rs={prof_meta['rs']:.2f} tau={prof_meta['tau']:.4f}) ===")

        # Regenere le tx_info deterministique
        mod = MOD_FACTORIES[prof_meta["mod_name"]]()
        rng = np.random.default_rng(prof_meta["seed"])
        tx = build_tx(prof_meta["rs"], prof_meta["beta"], prof_meta["tau"],
                      mod, n_data_symbols=prof_meta["n_data_symbols"], rng=rng)

        # Fenetre de recherche autour du debut attendu du profil
        # (le decalage TX/RX peut etre jusqu'a qq centaines de ms)
        coarse_start_s = prof_meta["profile_start_s"]
        search_window_s = prof_meta["profile_end_s"] - coarse_start_s + 2.0

        i0 = max(0, int((coarse_start_s - 1.0) * AUDIO_RATE))
        i1 = min(len(rx_audio),
                 int((prof_meta["profile_end_s"] + 1.0) * AUDIO_RATE))
        if i1 <= i0 + 100:
            print(f"  !! segment trop court, skip")
            continue
        segment = rx_audio[i0:i1]

        try:
            result = decode_rx_audio(segment, tx, mod)
        except Exception as e:
            print(f"  !! decode echec : {e}")
            continue

        ber = result.get("ber_uncoded")
        ser = result.get("ser")
        gmi = result.get("gmi")
        evm = result.get("evm_rms")
        sigma2 = result.get("sigma2", 0.0)
        n_proc = result["fse_out"]["n_processed"]
        snr_db = 10.0 * np.log10(1.0 / sigma2) if sigma2 > 0 else float("nan")

        print(f"  BER uncoded : {ber if ber is not None else 'nan'}")
        print(f"  SER          : {ser if ser is not None else 'nan'}")
        print(f"  GMI          : {gmi if gmi is not None else 'nan'} / "
              f"{mod.bits_per_sym}")
        print(f"  EVM          : {evm if evm is not None else 'nan'}")
        print(f"  SNR sortie   : {snr_db:.2f} dB")
        print(f"  Symboles decodes : {n_proc}")

        # Plot constellation
        const_path = os.path.join(out_dir, f"ota_{name}_constellation.png")
        try:
            mask = result["fse_out"]["training_mask"]
            outputs = result["fse_out"]["outputs"]
            plot_constellation(outputs, mod.constellation, const_path,
                               title=f"OTA {name} {mod.name} "
                                     f"Rs={prof_meta['rs']:.0f} "
                                     f"tau={prof_meta['tau']:.3f}",
                               mask=mask)
        except Exception as e:
            print(f"  plot constellation failed : {e}")

        results.append({
            "profile": name,
            "rs": prof_meta["rs"],
            "beta": prof_meta["beta"],
            "tau": prof_meta["tau"],
            "mod": mod.name,
            "ber": ber,
            "ser": ser,
            "gmi": gmi,
            "gmi_max": mod.bits_per_sym,
            "evm": evm,
            "snr_db": snr_db,
            "n_processed": n_proc,
        })

        with open(csv_path, "a", encoding="utf-8") as f:
            f.write(f"{name},{prof_meta['rs']:.2f},{prof_meta['beta']},"
                    f"{prof_meta['tau']:.4f},{mod.name},"
                    f"{ber if ber is not None else float('nan'):.6f},"
                    f"{ser if ser is not None else float('nan'):.6f},"
                    f"{gmi if gmi is not None else float('nan'):.4f},"
                    f"{mod.bits_per_sym},"
                    f"{evm if evm is not None else float('nan'):.4f},"
                    f"{snr_db:.3f},{n_proc}\n")

    # Resume
    json_path = os.path.join(out_dir, "ota_cascade_results.json")
    with open(json_path, "w", encoding="utf-8") as f:
        json.dump({"recording": os.path.abspath(args.recording),
                   "results": results}, f, indent=2, default=str)

    print(f"\n=== Resume ===")
    print(f"CSV        : {csv_path}")
    print(f"JSON       : {json_path}")
    print(f"Constellations : {out_dir}/ota_*_constellation.png")


if __name__ == "__main__":
    main()
