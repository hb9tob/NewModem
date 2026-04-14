# Scripts Reference

Tous les scripts se lancent avec le Python radioconda pour avoir GNU Radio :

```bash
PYTHONIOENCODING=utf-8 /c/Users/tous/radioconda/python.exe study/<script>.py
```

Le `PYTHONIOENCODING=utf-8` est nécessaire sur Windows pour que les caractères
accentués dans les `print` passent.

## Simulateur

### `nbfm_channel_sim.py`

Fait passer un WAV audio par le canal NBFM simulé.

```bash
python study/nbfm_channel_sim.py input.wav output.wav \
    --if-noise 0.165 \
    --drift-ppm -16 \
    --thermal-ppm 5 --thermal-period 180 \
    --start-delay 3.0 \
    --seed 42
```

Voir [NBFM Channel Simulator](NBFM-Channel-Simulator).

## Caractérisation du canal

### `generate_test_wav.py` / `generate_level_sweep_wav.py`

Génère un WAV multitone de test. La version `level_sweep` balaye 11 niveaux
de 0 à -20 dB.

```bash
python study/generate_level_sweep_wav.py
# → results/nbfm_level_sweep.wav
```

### `analyse_received_wav.py`

Analyse un WAV reçu de la version simple (un seul niveau).

```bash
python study/analyse_received_wav.py results/result_nbfm_test_multitone.wav
```

### `analyse_level_sweep.py`

Compare mode voice et mode data sur le balayage de niveau.

```bash
python study/analyse_level_sweep.py
# → résultats dans results/sim_validation_*.png
```

### `analyse_phase.py`

Analyse la réponse en phase et le group delay par niveau.

```bash
python study/analyse_phase.py
# → results/nbfm_phase_*.png
```

### `validate_simulator.py`

Compare le simulateur aux enregistrements réels (voice et data mode).

```bash
python study/nbfm_channel_sim.py results/nbfm_level_sweep.wav \
    results/sim_nbfm_level_sweep.wav --if-noise 0.165 --drift-ppm -16
python study/validate_simulator.py
# → results/sim_validation_*.png
```

## Bancs modem

### `pilot_placement_bench.py`

Teste différentes positions d'un pilote CW unique (400 à 1800 Hz).

```bash
python study/pilot_placement_bench.py
# → results/pilot_placement_{metrics,spectra}.png
```

### `dual_pilot_bench.py`

Compare mono pilote vs dual pilotes (4 scénarios).

```bash
python study/dual_pilot_bench.py
# → results/dual_pilot_{summary,traces}.png
```

### `modem_ber_bench.py`

BER complet : 8PSK / 16QAM / 32QAM vs symbol rate (500-2000 Bd).

```bash
python study/modem_ber_bench.py
# → results/modem_{ber_vs_rs,constellations}.png
```

Voir [BER Benchmarks](BER-Benchmarks).

## Ordre suggéré pour reproduire

1. Générer le WAV de test : `generate_level_sweep_wav.py`
2. (Hors workflow : jouer et enregistrer sur un vrai TX/RX NBFM)
3. Simuler : `nbfm_channel_sim.py results/nbfm_level_sweep.wav results/sim_nbfm_level_sweep.wav`
4. Valider : `validate_simulator.py`
5. Bancs modem : `pilot_placement_bench.py`, `dual_pilot_bench.py`, `modem_ber_bench.py`

## Dépendances

- Python 3.10 (radioconda)
- GNU Radio 3.10.4 (blocs `analog.nbfm_tx`, `nbfm_rx`, `noise_source_c`)
- numpy 1.23
- scipy 1.9
- matplotlib 3.6
