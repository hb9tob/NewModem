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
    slice_nearest,
)


def diagnose_modulator_clipping(outputs, mod, data_mask):
    """Detecte les signes de clipping/saturation cote TX modulateur FM,
    en analysant la geometrie du signal RECU (pas les dBFS carte son).

    Signaux diagnostiques :

    1) Ring ratio (16-APSK seul) : rapport mesure r2/r1 vs r2/r1 ideal
       (= gamma=2.85). Si le modulateur compresse les pics, l'anneau
       exterieur est "aplati" -> ratio mesure < gamma ideal.
       Flag si ecart > 15 %.

    2) Envelope peak compression : 99e percentile de |y| mesure vs
       attendu (Es=1 moyenne). Pour constellation sans clip, on a
       typiquement |y|_max ~ 1.2..1.5. Si le 99e percentile est
       "ecretee" (grande concentration au meme niveau), c'est suspect.

    3) EVM outer-vs-inner (16-APSK) : EVM sur points outer doit etre
       du meme ordre que EVM inner. Si outer EVM >> inner, clipping.

    Retourne un dict avec les chiffres + un flag `clipping_suspected`.
    """
    y = outputs[~data_mask] if data_mask is not None else outputs
    idx = slice_nearest(y, mod)
    const = mod.constellation
    radii = np.abs(const)
    has_two_rings = len(np.unique(np.round(radii, 3))) == 2

    diag = {
        "clipping_suspected": False,
        "reasons": [],
    }

    # --- 1) Ring ratio (seulement pour 16-APSK) ---
    if has_two_rings and mod.name == "16-APSK":
        inner_radius_ideal = radii.min()
        outer_radius_ideal = radii.max()
        gamma_ideal = outer_radius_ideal / inner_radius_ideal

        inner_points = np.where(radii < np.median(radii))[0]
        outer_points = np.where(radii > np.median(radii))[0]

        inner_mags = []
        outer_mags = []
        for k in inner_points:
            sel = y[idx == k]
            if len(sel) >= 5:
                inner_mags.append(np.mean(np.abs(sel)))
        for k in outer_points:
            sel = y[idx == k]
            if len(sel) >= 5:
                outer_mags.append(np.mean(np.abs(sel)))

        if inner_mags and outer_mags:
            r1_meas = float(np.mean(inner_mags))
            r2_meas = float(np.mean(outer_mags))
            gamma_meas = r2_meas / r1_meas if r1_meas > 0 else 0
            gamma_error_pct = 100 * abs(gamma_meas - gamma_ideal) / gamma_ideal
            diag["gamma_ideal"] = gamma_ideal
            diag["gamma_measured"] = gamma_meas
            diag["gamma_error_pct"] = gamma_error_pct
            if gamma_error_pct > 15:
                diag["clipping_suspected"] = True
                diag["reasons"].append(
                    f"ring ratio mesure {gamma_meas:.2f} vs {gamma_ideal:.2f} "
                    f"ideal (ecart {gamma_error_pct:.1f}%) "
                    f"-> compression/clipping TX probable"
                )

    # --- 2) Envelope peak dispersion ---
    # Pour constellations a module CONSTANT (QPSK, 8PSK), tous les symboles
    # ont meme |y|. Une variation p99/p50 > 1.2 indique une distorsion non
    # prevue (mod clipping non-lineaire, bruit anormal). Pour 16-APSK le
    # ratio theorique depend des proportions inner/outer (75% outer) et
    # tombe ~1.0 ; la compression se voit mieux via ring ratio (cas 1) et
    # EVM out/in (cas 3). On publie le chiffre mais on ne l'utilise comme
    # critere de flag que pour constant-modulus.
    envelope = np.abs(y)
    p99 = float(np.percentile(envelope, 99))
    p50 = float(np.percentile(envelope, 50))
    p99_over_p50 = p99 / p50 if p50 > 0 else 0
    diag["envelope_p50"] = p50
    diag["envelope_p99"] = p99
    diag["envelope_p99_over_p50"] = p99_over_p50
    if mod.name in ("QPSK", "8PSK") and p99_over_p50 > 1.25:
        diag["clipping_suspected"] = True
        diag["reasons"].append(
            f"mod constant-modulus ({mod.name}) mais p99/p50 envelope = "
            f"{p99_over_p50:.2f} (>1.25) -> distorsion non-lineaire TX"
        )

    # --- 3) EVM outer vs inner (16-APSK) ---
    if has_two_rings and mod.name == "16-APSK":
        inner_evm_sq = []
        outer_evm_sq = []
        for k in inner_points:
            sel = y[idx == k]
            if len(sel) >= 5:
                err = sel - const[k]
                inner_evm_sq.append(np.mean(np.abs(err) ** 2))
        for k in outer_points:
            sel = y[idx == k]
            if len(sel) >= 5:
                err = sel - const[k]
                outer_evm_sq.append(np.mean(np.abs(err) ** 2))
        if inner_evm_sq and outer_evm_sq:
            evm_inner = np.sqrt(np.mean(inner_evm_sq))
            evm_outer = np.sqrt(np.mean(outer_evm_sq))
            diag["evm_inner"] = float(evm_inner)
            diag["evm_outer"] = float(evm_outer)
            ratio = evm_outer / evm_inner if evm_inner > 0 else 0
            diag["evm_outer_over_inner"] = float(ratio)
            # En canal AWGN uniforme, ratio = 1. Si outer >> inner (ratio>2),
            # c'est signe de clipping des pics outer.
            if ratio > 2.0:
                diag["clipping_suspected"] = True
                diag["reasons"].append(
                    f"EVM outer/inner = {ratio:.2f} (>2) "
                    f"-> distorsion specifique aux pics outer = clip TX"
                )

    return diag


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

    # Detection globale de l'offset TX : le WAV TX prevoit 3 s de silence
    # initial + 0.5 s marker + data. L'OTA commence souvent + tard (PTT,
    # demarrage lecteur). On cherche la 1ere activite significative et on
    # cale tout le metadata dessus.
    win_s = 0.1
    win = int(win_s * AUDIO_RATE)
    n_rx = len(rx_audio)
    rms = np.array([
        np.sqrt(np.mean(rx_audio[i:i+win] ** 2))
        for i in range(0, n_rx - win, win // 2)
    ])
    floor = np.percentile(rms, 10)
    threshold = max(10.0 * floor, 0.02)
    # Cherche la 1ere region SOUTENUE (>= 5 fenetres consecutives = 250 ms)
    # au-dessus du seuil, pour eviter les clicks / glitches isoles.
    n_consec_required = 5
    above = rms > threshold
    first_sustained = -1
    run_len = 0
    for i, a in enumerate(above):
        if a:
            run_len += 1
            if run_len >= n_consec_required:
                first_sustained = i - n_consec_required + 1
                break
        else:
            run_len = 0
    if first_sustained < 0:
        print(f"!! Aucune activite soutenue detectee (seuil {threshold:.4f}), "
              f"analyse sans alignement global.")
        rx_tx_offset_s = 0.0
    else:
        rx_tx_start_s = first_sustained * (win_s / 2.0)
        meta_mega_start = meta["profiles"][0].get("marker_start_s")
        if meta_mega_start is None:
            meta_mega_start = meta["profiles"][0]["profile_start_s"]
        rx_tx_offset_s = rx_tx_start_s - meta_mega_start
        print(f"Alignement : 1ere activite RX a {rx_tx_start_s:.2f} s ; "
              f"metadata prevoit {meta_mega_start:.2f} s ; "
              f"offset applique = {rx_tx_offset_s:+.2f} s")

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

        # Fenetre de recherche autour du debut attendu, offset global applique
        coarse_start_s = prof_meta["profile_start_s"] + rx_tx_offset_s
        coarse_end_s = prof_meta["profile_end_s"] + rx_tx_offset_s

        i0 = max(0, int((coarse_start_s - 1.0) * AUDIO_RATE))
        i1 = min(len(rx_audio),
                 int((coarse_end_s + 1.0) * AUDIO_RATE))
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

        # --- Diagnostic clipping modulateur FM ---
        try:
            mask = result["fse_out"]["training_mask"]
            outputs = result["fse_out"]["outputs"]
            diag = diagnose_modulator_clipping(outputs, mod, mask)
            if diag.get("gamma_ideal") is not None:
                print(f"  Ring ratio   : mesure {diag['gamma_measured']:.2f} "
                      f"vs ideal {diag['gamma_ideal']:.2f} "
                      f"(ecart {diag['gamma_error_pct']:.1f}%)")
            if diag.get("evm_outer_over_inner") is not None:
                print(f"  EVM out/in   : "
                      f"{diag['evm_outer_over_inner']:.2f} (1.0 = pas de clip)")
            print(f"  Env p99/p50  : {diag['envelope_p99_over_p50']:.2f}")
            if diag["clipping_suspected"]:
                print(f"  !! CLIPPING TX SUSPECTE :")
                for r in diag["reasons"]:
                    print(f"     - {r}")
            else:
                print(f"  clip diag   : OK (pas de clip detecte)")
        except Exception as e:
            diag = {"error": str(e)}
            print(f"  diag clip failed: {e}")

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
            "clip_diagnostic": diag,
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
