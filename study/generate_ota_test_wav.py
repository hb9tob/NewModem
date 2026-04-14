#!/usr/bin/env python3
"""
Genere un WAV long pour validation OTA du modem sur canal NBFM reel.

Contenu :
  1. Silence initial (5 s) : plancher de bruit avant TX
  2. Marker sync : pilotes 400/1800 Hz forts (2 s)
  3. Silence (2 s)
  4. Pilotes seuls, niveau nominal (10 s) : mesure SNR pilotes OTA
  5. Silence (2 s)
  6. Pour chaque (modulation, symbol_rate) :
      preambule QPSK (0.5 s) + data modulee (15 s) + gap (1 s)
     Tests : 8PSK/16QAM/32QAM a 500, 1000 Bd (zone propre)
             16QAM/32QAM a 1200 et 1500 Bd (stress, ou on s'attend a casser)
  7. Marker sync final (2 s)
  8. Silence final (3 s)

Tous les parametres TX (seeds, preambules) sont sauvegardes dans
results/ota_test_timeline.json pour permettre l'analyse apres enregistrement.
"""

import os, sys, json
import numpy as np
import wave

sys.path.insert(0, os.path.dirname(__file__))
from modem_ber_bench import (
    AUDIO_RATE, PILOT_FREQS, PILOT_REL_DB, DATA_CENTER, ROLLOFF,
    CONSTELLATIONS, build_tx, N_PREAMBLE_SYMBOLS
)
from pilot_placement_bench import rrc_taps

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
OUTPUT_PREFIX = os.path.join(RESULTS_DIR, "ota_test")
MAX_FILE_DURATION_S = 5 * 60

SYNC_MARKER_AMP = 0.6   # pilotes forts pour reperer debut/fin
PILOT_BLOCK_AMP = 0.4   # pilotes seuls pour mesure SNR
SILENCE_INITIAL = 5.0
SILENCE_FINAL = 3.0
SILENCE_BETWEEN = 2.0
GAP_BETWEEN_TESTS = 1.0
PILOT_BLOCK_DURATION = 10.0
MARKER_DURATION = 2.0
TEST_DATA_DURATION = 6.0       # duree du signal modem (preamble + data)
                                # limite par le demod actuel (sans timing
                                # dynamique) vs derive d'horloge.

# Niveaux a tester (dB, relatifs au max 0.9 crete). On cherche le point de
# saturation du modulateur FM (hard clip sur l'excursion).
TEST_LEVELS_DB = [0.0, -3.0, -6.0, -10.0, -15.0]

# Combinaisons modulation/Rs a tester a chaque niveau.
TEST_MOD_RS = [
    ("8PSK",  750),
    ("8PSK",  1000),
    ("16QAM", 750),
    ("16QAM", 1000),
    ("32QAM", 500),
    ("32QAM", 750),
]
# Expansion : (mod, rs, seed) pour chaque niveau
TEST_CONFIGS = [
    (mod, rs, 100 + 10 * i + j, lvl)
    for j, lvl in enumerate(TEST_LEVELS_DB)
    for i, (mod, rs) in enumerate(TEST_MOD_RS)
]


def silence(duration_s):
    return np.zeros(int(duration_s * AUDIO_RATE), dtype=np.float32)


def pilot_block(duration_s, amplitude, seed=0):
    rng = np.random.RandomState(seed)
    n = int(duration_s * AUDIO_RATE)
    t = np.arange(n) / AUDIO_RATE
    amp_each = amplitude / np.sqrt(len(PILOT_FREQS))
    sig = np.zeros(n, dtype=np.float32)
    for fp in PILOT_FREQS:
        sig += amp_each * np.cos(
            2 * np.pi * fp * t + rng.uniform(0, 2 * np.pi))
    # Rampe cosinus 20 ms aux bords
    ramp = int(0.020 * AUDIO_RATE)
    if ramp * 2 < n:
        r = 0.5 * (1 - np.cos(np.pi * np.arange(ramp) / ramp))
        sig[:ramp] *= r; sig[-ramp:] *= r[::-1]
    return sig.astype(np.float32)


def build_test_block(mod_name, symbol_rate, seed, level_db=0.0):
    """Reutilise build_tx, ajuste duree, puis applique le niveau."""
    rng = np.random.RandomState(seed)
    target_sym = int(TEST_DATA_DURATION * symbol_rate) - N_PREAMBLE_SYMBOLS
    import modem_ber_bench as mbb
    saved = mbb.N_DATA_SYMBOLS
    mbb.N_DATA_SYMBOLS = target_sym
    try:
        sig, data_syms, data_idx, pre_syms, sps, taps = build_tx(
            symbol_rate, mod_name, rng)
    finally:
        mbb.N_DATA_SYMBOLS = saved

    # Applique le niveau : scale global (data + pilotes ensemble)
    gain = 10 ** (level_db / 20.0)
    sig = sig * gain

    # Rampe douce pour eviter clicks aux transitions
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
        "sps": sps,
        "data_idx": [int(x) for x in data_idx.tolist()],
    }


def write_wav(filename, samples):
    s = np.clip(samples * 32767.0, -32768, 32767).astype(np.int16)
    with wave.open(filename, "w") as wf:
        wf.setnchannels(1); wf.setsampwidth(2); wf.setframerate(AUDIO_RATE)
        wf.writeframes(s.tobytes())


class FilePart:
    """Accumule des segments audio + timeline, jusqu'a saturation de duree."""
    def __init__(self, name):
        self.name = name
        self.segments = []
        self.timeline = []
        self.duration = 0.0

    def add(self, label, samples, **info):
        dur = len(samples) / AUDIO_RATE
        self.timeline.append({
            "label": label,
            "start_s": self.duration,
            "end_s": self.duration + dur,
            **info,
        })
        self.segments.append(samples)
        self.duration += dur


def finalize_part(part, index):
    """Ajoute un marker de fin et sauvegarde le fichier."""
    part.add("marker_end",
             pilot_block(MARKER_DURATION, SYNC_MARKER_AMP, seed=3),
             pilot_freqs=list(PILOT_FREQS))
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
            "pilot_freqs": list(PILOT_FREQS),
            "pilot_rel_db": PILOT_REL_DB,
            "data_center": DATA_CENTER,
            "rolloff": ROLLOFF,
            "part": index,
            "part_name": part.name,
            "duration_s": part.duration,
            "timeline": part.timeline,
        }, f, indent=2)
    print(f"  -> {os.path.basename(wav_path)}  ({part.duration:.1f} s)")
    return wav_path, json_path


def new_part(name):
    p = FilePart(name)
    p.add("silence_initial", silence(SILENCE_INITIAL))
    p.add("marker_start", pilot_block(MARKER_DURATION, SYNC_MARKER_AMP, seed=1),
          pilot_freqs=list(PILOT_FREQS))
    p.add("silence", silence(SILENCE_BETWEEN))
    return p


def main():
    os.makedirs(RESULTS_DIR, exist_ok=True)
    print("=== Generation WAVs OTA (multi-fichiers) ===")
    print(f"Duree max par fichier : {MAX_FILE_DURATION_S/60:.1f} min\n")

    files = []

    # --- Part 1 : reference (bruit + pilotes seuls) ---
    print(f"Part 01 : reference")
    p = new_part("reference")
    p.add("pilot_only",
          pilot_block(PILOT_BLOCK_DURATION, PILOT_BLOCK_AMP, seed=2),
          pilot_freqs=list(PILOT_FREQS), amplitude=PILOT_BLOCK_AMP)
    p.add("silence", silence(SILENCE_BETWEEN))
    files.append(finalize_part(p, len(files) + 1))

    # --- Parts 2..N : blocs modem avec balayage de niveau ---
    # Ordre : on groupe par niveau pour qu'une meme part contienne plusieurs
    # modulations au meme niveau (plus facile a analyser si on joue part par part)
    current = None
    for mod, rs, seed, level_db in TEST_CONFIGS:
        print(f"  prepare {mod} @ {rs} Bd niveau {level_db:+.0f} dB...", end=" ")
        sig, info = build_test_block(mod, rs, seed, level_db=level_db)
        block_dur = len(sig) / AUDIO_RATE + GAP_BETWEEN_TESTS
        if current is None or current.duration + block_dur + MARKER_DURATION + SILENCE_FINAL > MAX_FILE_DURATION_S:
            if current is not None:
                files.append(finalize_part(current, len(files) + 1))
            part_name = f"levelsweep{len(files)+1:02d}"
            print(f"-> nouvelle part {part_name}")
            current = new_part(part_name)
        else:
            print("(ajoute)")
        label = f"modem_{mod}_{rs}Bd_{level_db:+.0f}dB"
        current.add(label, sig, **info)
        current.add("gap", silence(GAP_BETWEEN_TESTS))

    if current is not None and len(current.segments) > 3:
        files.append(finalize_part(current, len(files) + 1))

    total = sum(os.path.getsize(f[0]) for f in files) / (1024 * 1024)
    print(f"\n{len(files)} fichiers generes ({total:.1f} MiB total)")
    for wav, _ in files:
        print(f"  {os.path.basename(wav)}")

    print("\nProcedure OTA :")
    print("1. Jouer chaque ota_test_partNN_*.wav sur l'entree audio TX")
    print("   (un fichier par session d'enregistrement si tu veux, ou a la suite)")
    print("2. Enregistrer chaque sortie RX en WAV 48 kHz mono 16-bit")
    print("3. Analyser : python study/analyse_ota_recording.py <recording.wav> "
          "--timeline <part.json>")


if __name__ == "__main__":
    main()
