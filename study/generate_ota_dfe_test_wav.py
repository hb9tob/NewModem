#!/usr/bin/env python3
"""
Genere un WAV de test OTA pour le modem avec pilotes TDM + DFE.
Tests Rs > 1000 Bd dans la zone sweet-spot decouverte en simulation.
Deux niveaux (-3 dB et -6 dB, d'apres le balayage OTA precedent).
"""

import os, sys, json
import numpy as np
import wave

sys.path.insert(0, os.path.dirname(__file__))
from modem_tdm_ber_bench import (
    AUDIO_RATE, build_tx, D_SYMS, P_SYMS, N_PREAMBLE_SYMBOLS,
    DATA_CENTER, ROLLOFF
)

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
OUTPUT_PREFIX = os.path.join(RESULTS_DIR, "ota_dfe_test")
MAX_FILE_DURATION_S = 5 * 60

# Parametres communs
SYNC_MARKER_AMP = 0.6
PILOT_BLOCK_AMP = 0.4
SILENCE_INITIAL = 5.0
SILENCE_FINAL = 3.0
SILENCE_BETWEEN = 2.0
GAP_BETWEEN_TESTS = 1.0
MARKER_DURATION = 2.0
TEST_DATA_DURATION = 6.0

# Deux niveaux dans le sweet spot
TEST_LEVELS_DB = [-3.0, -6.0]

# Configurations modem/Rs a tester (dans la zone sweet sim avec DFE)
TEST_MOD_RS = [
    ("8PSK",  1500),
    ("8PSK",  1600),
    ("8PSK",  2000),   # 8PSK a 2000 Bd : BER 5.6e-5 en sim, a verifier OTA
    ("16QAM", 1200),
    ("16QAM", 1500),
    ("16QAM", 1600),
    ("32QAM", 1000),
    ("32QAM", 1200),
    ("32QAM", 1500),
]

# Expansion : (mod, rs, seed, level)
TEST_CONFIGS = [
    (mod, rs, 200 + 10 * i + j, lvl)
    for j, lvl in enumerate(TEST_LEVELS_DB)
    for i, (mod, rs) in enumerate(TEST_MOD_RS)
]


# --- Fonctions auxiliaires (markers, silence) ---
def silence(duration_s):
    return np.zeros(int(duration_s * AUDIO_RATE), dtype=np.float32)


def pilot_block(duration_s, amplitude, seed=0):
    rng = np.random.RandomState(seed)
    n = int(duration_s * AUDIO_RATE)
    t = np.arange(n) / AUDIO_RATE
    # Pilotes a 400 et 1800 Hz uniquement pour les markers, cosines
    sig = np.zeros(n, dtype=np.float32)
    for fp in [400.0, 1800.0]:
        sig += amplitude / np.sqrt(2) * np.cos(
            2 * np.pi * fp * t + rng.uniform(0, 2 * np.pi))
    ramp = int(0.020 * AUDIO_RATE)
    if ramp * 2 < n:
        r = 0.5 * (1 - np.cos(np.pi * np.arange(ramp) / ramp))
        sig[:ramp] *= r; sig[-ramp:] *= r[::-1]
    return sig.astype(np.float32)


def build_modem_block(mod_name, symbol_rate, seed, level_db=0.0):
    """Build TX via modem_tdm_ber_bench.build_tx, applique le niveau."""
    rng = np.random.RandomState(seed)
    target_sym = int(TEST_DATA_DURATION * symbol_rate) - N_PREAMBLE_SYMBOLS
    import modem_tdm_ber_bench as mtb
    saved = mtb.N_DATA_SYMBOLS
    mtb.N_DATA_SYMBOLS = target_sym
    try:
        sig, data_syms, data_idx, pre_syms, pilot_pos, sps, taps = build_tx(
            symbol_rate, mod_name, rng)
    finally:
        mtb.N_DATA_SYMBOLS = saved

    # Applique niveau
    gain = 10 ** (level_db / 20.0)
    sig = sig * gain

    # Rampe
    ramp = int(0.010 * AUDIO_RATE)
    if ramp * 2 < len(sig):
        r = 0.5 * (1 - np.cos(np.pi * np.arange(ramp) / ramp))
        sig[:ramp] *= r; sig[-ramp:] *= r[::-1]

    return sig, {
        "mod": mod_name,
        "symbol_rate": symbol_rate,
        "seed": seed,
        "level_db": level_db,
        "n_preamble_symbols": N_PREAMBLE_SYMBOLS,
        "n_data_symbols": target_sym,
        "d_syms": D_SYMS,
        "p_syms": P_SYMS,
        "tdm": True,
        "sps": sps,
        "data_idx": [int(x) for x in data_idx.tolist()],
    }


def write_wav(filename, samples):
    s = np.clip(samples * 32767.0, -32768, 32767).astype(np.int16)
    with wave.open(filename, "w") as wf:
        wf.setnchannels(1); wf.setsampwidth(2); wf.setframerate(AUDIO_RATE)
        wf.writeframes(s.tobytes())


class FilePart:
    def __init__(self, name):
        self.name = name
        self.segments = []
        self.timeline = []
        self.duration = 0.0

    def add(self, label, samples, **info):
        dur = len(samples) / AUDIO_RATE
        self.timeline.append({
            "label": label, "start_s": self.duration,
            "end_s": self.duration + dur, **info,
        })
        self.segments.append(samples)
        self.duration += dur


def finalize_part(part, index):
    part.add("marker_end",
             pilot_block(MARKER_DURATION, SYNC_MARKER_AMP, seed=3))
    part.add("silence_final", silence(SILENCE_FINAL))
    audio = np.concatenate(part.segments)
    peak = np.max(np.abs(audio))
    if peak > 0.9:
        audio = audio * (0.9 / peak)
    wav_path = f"{OUTPUT_PREFIX}_part{index:02d}_{part.name}.wav"
    json_path = f"{OUTPUT_PREFIX}_part{index:02d}_{part.name}.json"
    write_wav(wav_path, audio)
    with open(json_path, "w") as f:
        json.dump({
            "audio_rate": AUDIO_RATE,
            "data_center": DATA_CENTER,
            "rolloff": ROLLOFF,
            "part": index, "part_name": part.name,
            "duration_s": part.duration,
            "timeline": part.timeline,
        }, f, indent=2)
    print(f"  -> {os.path.basename(wav_path)}  ({part.duration:.1f} s)")
    return wav_path, json_path


def new_part(name):
    p = FilePart(name)
    p.add("silence_initial", silence(SILENCE_INITIAL))
    p.add("marker_start", pilot_block(MARKER_DURATION, SYNC_MARKER_AMP, seed=1))
    p.add("silence", silence(SILENCE_BETWEEN))
    return p


def main():
    os.makedirs(RESULTS_DIR, exist_ok=True)
    print("=== Generation WAV OTA DFE (TDM pilotes) ===")
    print(f"Max {MAX_FILE_DURATION_S/60:.1f} min/fichier, "
          f"{len(TEST_CONFIGS)} blocs total\n")

    files = []
    current = None
    for mod, rs, seed, lvl in TEST_CONFIGS:
        print(f"  prepare {mod} @ {rs} Bd niveau {lvl:+.0f} dB...", end=" ")
        sig, info = build_modem_block(mod, rs, seed, level_db=lvl)
        block_dur = len(sig) / AUDIO_RATE + GAP_BETWEEN_TESTS
        if current is None or current.duration + block_dur + MARKER_DURATION + SILENCE_FINAL > MAX_FILE_DURATION_S:
            if current is not None:
                files.append(finalize_part(current, len(files) + 1))
            part_name = f"dfe{len(files)+1:02d}"
            print(f"-> nouvelle part {part_name}")
            current = new_part(part_name)
        else:
            print("(ajoute)")
        label = f"dfe_{mod}_{rs}Bd_{lvl:+.0f}dB"
        current.add(label, sig, **info)
        current.add("gap", silence(GAP_BETWEEN_TESTS))

    if current is not None and len(current.segments) > 3:
        files.append(finalize_part(current, len(files) + 1))

    total = sum(os.path.getsize(f[0]) for f in files) / (1024 * 1024)
    print(f"\n{len(files)} fichiers generes ({total:.1f} MiB total)")
    for wav, _ in files:
        print(f"  {os.path.basename(wav)}")

    print("\nProcedure :")
    print("1. Jouer chaque fichier sur le TX au meme niveau que levelsweep")
    print("2. Enregistrer la sortie RX")
    print("3. Analyser : python study/analyse_ota_dfe_recording.py <rec.wav> "
          "--timelines <part.json>")


if __name__ == "__main__":
    main()
