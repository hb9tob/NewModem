# NBFM Channel Simulator

Simulateur de canal basé sur les blocs GNU Radio `analog.nbfm_tx` et
`analog.nbfm_rx`, calibré sur les mesures réelles.

## Architecture

```
audio_in → hard-clip TX → HPF 300 Hz → nbfm_tx → + bruit IF complexe → nbfm_rx
                                                                     → LPF 2400 Hz
                                                                     → gain -6.5 dB
                                                                     → clock drift (ppm)
                                                                     → audio_out
```

Le bruit est injecté **à l'IF** (domaine complexe, entre pré-emphase/modulation
et démodulation/de-emphase), ce qui modélise fidèlement le bruit thermique du
récepteur FM.

## Paramètres

| Paramètre | Défaut | Rôle |
|---|---|---|
| `audio_rate` | 48000 Hz | échantillonnage audio |
| `quad_rate` | 480000 Hz | échantillonnage IF |
| `max_dev` | 5000 Hz | déviation FM max |
| `tau` | 75 µs | pré/de-emphase |
| `sub_audio_hpf` | 300 Hz | filtre CTCSS |
| `post_lpf` | 2400 Hz | LPF audio du transceiver |
| `post_gain_db` | -6.5 dB | calibration niveau |
| `tx_hard_clip` | 0.55 | seuil de hard-clip audio avant FM (simule limiteur d'excursion) |
| `if_noise` | 0.165 | std bruit IF (cible ~30 dB SNR audio) |
| `drift_ppm` | -16 ppm | dérive horloge statique |
| `thermal_ppm` | 0 ppm | amplitude variation thermique |
| `thermal_period_s` | 120 s | période variation thermique |
| `start_delay_s` | aléatoire [2,5] | délai TX→RX initial |

## Validation

Mesures comparées aux enregistrements réels en mode data :

| Métrique | Sim | Réel | Écart |
|---|---|---|---|
| Plancher bruit (gap) | 0.00114 | 0.00113 | **<1 %** |
| Gain moyen 300-2200 Hz | -6.82 dB | -6.51 dB | 0.3 dB |
| Dérive group delay / 55 s | -0.88 ms | -0.88 ms | **exact** |
| RMS écart réponse fréquentielle | — | — | **1.2 dB** |
| Linéarité 0 à -20 dB | ±0.02 dB | ±0.07 dB | — |

Le mode voice (avec compresseur/limiteur audio) **n'est pas modélisé** —
ajouter un compresseur audio en amont du TX pour le simuler.

## Hard-clip TX (limiteur FM)

Le modulateur FM des transceivers hard-clippe l'amplitude audio pour limiter
l'excursion de fréquence sous la bande licenciée. Sur les signaux à PAPR
non-négligeable (QAM dense), ce clip dégrade le BER si le niveau d'entrée
est trop élevé.

**Validation OTA** : 32QAM 750 Bd passe de BER 7.2e-3 à 0 dB vers **4.7e-5**
à -6 dB, soit un facteur 150 d'amélioration. Le sim reproduit cet effet
qualitativement avec `tx_hard_clip=0.55`.

Désactiver avec `--tx-clip 0`.

## Usage

```bash
python study/nbfm_channel_sim.py input.wav output.wav \
    --if-noise 0.165 \
    --drift-ppm -16 \
    --thermal-ppm 5 --thermal-period 180 \
    --start-delay 3.0 \
    --seed 42
```

- `--if-noise` : amplitude bruit IF (0.165 = canal nominal mesuré)
- `--drift-ppm` : dérive statique
- `--thermal-ppm` : variation sinusoïdale thermique (0 = désactivé)
- `--start-delay` : délai initial fixe (ou omettre pour aléatoire 2-5s)
- `--seed` : reproductibilité

## Dérive d'horloge

Le modèle de dérive applique une interpolation temporelle avec un taux
instantané :

```
rate(t) = drift_ppm × 10⁻⁶ + thermal_ppm × 10⁻⁶ × sin(2π t / period)
```

puis interpole le signal à des instants déplacés par l'intégrale cumulée de
ce taux. Réplique fidèlement le décalage d'horloge observé entre soundcards
(mesuré à -16 ppm sur le setup réel).

## Délai TX→RX

Le simulateur ajoute par défaut un **délai aléatoire de 2 à 5 s** en début
de transmission (silence TX, porteuse + bruit IF actifs). Ceci force les
tests de récepteur à faire une vraie synchronisation plutôt que de
bénéficier d'un alignement artificiel TX/RX bit-à-bit.
