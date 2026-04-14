# NewModem

Modem audio pour transmission d'images via relais NBFM radio amateur.

Conception d'un modem single-carrier broadcast (point-multipoint, sans ARQ)
visant un débit de 3-4 kb/s net sur les relais NBFM voix.

## Canal

Les transceivers NBFM amateurs appliquent pré-emphase à l'émission et
dé-emphase à la réception. Le canal a été caractérisé en voice et data
mode — voir [rapport canal](rapport_canal_nbfm.html).

Principales caractéristiques mesurées :

| Paramètre | Valeur |
|---|---|
| Bande utile (-3 dB) | 300 – 2200 Hz |
| SNR audio mode data | ~30 dB |
| Dérive horloge soundcard | -16 ppm (mesurée) |
| Phase non-linéaire | ~40° std dans la bande |
| Mode voice | limiteur actif (compression ~1 dB) |

## Modem

Design single-carrier inspiré du modem QO-100 d'EA4GPZ (HSMODEM) :

- Shaping RRC α=0.25
- Constellations 8PSK / 16QAM / 32QAM
- **Dual pilotes** à 400 et 1800 Hz (tracking phase commune + group delay)
- Symbol rate max viable : **1000 Bd** avant collision pilote-data

Voir [rapport modem](rapport_modem.html) pour les détails, courbes BER et
constellations.

## Simulateur

Simulateur de canal NBFM basé sur les blocs GNU Radio `analog.nbfm_tx` /
`nbfm_rx`, avec :

- Injection de bruit gaussien complexe à l'IF (entre pré-emphase et de-emphase)
- HPF sub-audio 300 Hz (filtre CTCSS)
- LPF post-demod 2400 Hz
- Dérive d'horloge soundcard paramétrable (statique + variation thermique)
- Délai initial TX→RX aléatoire 2-5 s (force la resynchro)

Usage :

```bash
python study/nbfm_channel_sim.py input.wav output.wav \
    --if-noise 0.165 --drift-ppm -16 \
    --thermal-ppm 5 --thermal-period 180
```

## Environnement

- Python 3.10 via radioconda (`C:\Users\tous\radioconda\`)
- GNU Radio 3.10.4
- numpy 1.23, scipy 1.9, matplotlib 3.6

Exécution des scripts :

```bash
/c/Users/tous/radioconda/python.exe study/<script>.py
```

## Structure

```
study/           Scripts d'étude, simulateur, bancs de test modem
results/         WAV de mesure, CSV, graphiques générés
wiki/            Pages du wiki GitHub
rapport_*.html   Rapports HTML avec graphiques
```

## Scripts principaux

| Script | Rôle |
|---|---|
| `nbfm_channel_sim.py` | Simulateur de canal NBFM (GNU Radio + drift) |
| `generate_level_sweep_wav.py` | WAV multitone balayage de niveau |
| `analyse_level_sweep.py` | Analyse gain/BW par niveau |
| `analyse_phase.py` | Analyse réponse en phase |
| `validate_simulator.py` | Validation sim vs enregistrements réels |
| `pilot_placement_bench.py` | Optimisation position pilote CW |
| `dual_pilot_bench.py` | Comparaison mono vs dual pilotes |
| `modem_ber_bench.py` | BER 8PSK/16QAM/32QAM vs symbol rate |
