# BER Benchmarks

Banc de test BER sur canal NBFM simulé, au SNR audio ~30 dB, dérive -16 ppm.

## Conditions

- **TX** : préambule 256 symboles QPSK + 6000 symboles data, RRC α=0.25
- **Data center** : 1100 Hz
- **Pilotes** : 400 et 1800 Hz, -6 dB sous crête data
- **Canal** : simulateur NBFM, `if_noise=0.165`, `drift_ppm=-16`,
  `start_delay=3.0` s fixe
- **RX** : corrélation préambule pour timing, correction de phase via moyenne
  des pilotes, match RRC, slicer dur (pas de FEC)

## Résultats BER par modulation et symbol rate

| Modulation | 500 Bd | 750 Bd | 1000 Bd | 1200 Bd | 1500 Bd | 2000 Bd |
|---|---|---|---|---|---|---|
| **8PSK** | 0 | 0 | **0** | 5.1e-3 | 0.20 | 0.25 |
| **16QAM** | 0 | 0 | **0** | 3.2e-2 | 0.38 | 0.40 |
| **32QAM** | 1.5e-3 | 2.0e-3 | 2.2e-3 | 0.17 | 0.39 | 0.42 |
| EVM | ~9 % | ~9 % | ~9 % | ~26 % | ~73 % | ~85 % |

`0` = 0 erreur sur 18 000 à 30 000 bits testés.

## Le mur à 1200 Bd

Le symbol rate maximal viable est **1000 Bd**. Au-delà, le spectre data avec
α=0.25 déborde sur les pilotes :

| Rs | BW data | Plage spectrale | État pilotes |
|---|---|---|---|
| 1000 Bd | 1250 Hz | [475, 1725] Hz | propres |
| 1200 Bd | 1500 Hz | [350, 1850] Hz | **dans la bande data** |
| 1500 Bd | 1875 Hz | [162, 2037] Hz | + dépasse la bande utile |

L'interférence coherente du pilote sur les bins data dégrade le SNR effectif
par symbole de 10-15 dB → la constellation s'effondre.

Les constellations à 1200 Bd montrent des signatures géométriques en rayures
diagonales typiques d'une interférence CW sur un symbol period.

## Débit brut utilisable

À 1000 Bd sans FEC :

| Modulation | Débit brut | BER | FEC recommandée | Net |
|---|---|---|---|---|
| 8PSK | 3.0 kb/s | 0 | 3/4 (marge) | 2.25 kb/s |
| 16QAM | 4.0 kb/s | 0 | 3/4 | **3.0 kb/s** |
| 32QAM | 5.0 kb/s | 2e-3 | 2/3 | **3.33 kb/s** |

Le **16QAM** est le sweet spot : BER nul sans FEC, et 4 bits/symbole.

Le **32QAM** a un résidu BER 2e-3 dû au Gray non-optimal et à la phase
résiduelle. Corrigeable par FEC 2/3 pour un débit net supérieur.

## Comparaison avec les alternatives

| Modem | Débit net typique sur NBFM | Commentaire |
|---|---|---|
| HamDRM + turbo | ~2 kb/s | OFDM, casse en mode voice |
| HSMODEM (QO-100) | ~5 kb/s | mais canal SSB linéaire |
| VARA FM | 8-15 kb/s | propriétaire, pas broadcast |
| **Ce design** | **3-3.3 kb/s** | broadcast, ouvert, voice-compatible |

## Validation OTA (canal réel)

Le design a été validé en OTA (over-the-air) via un vrai TX NBFM, avec un
balayage de niveau sur les constellations 8PSK / 16QAM / 32QAM à 500, 750
et 1000 Bd.

### Niveau 0 dB (crête 0.9) — TX saturé

| Modulation | 500 Bd | 750 Bd | 1000 Bd |
|---|---|---|---|
| 8PSK | 0 | 0 | 1.2e-3 |
| 16QAM | 0 | 1.2e-4 | 2.1e-2 |
| 32QAM | 0 | 1.3e-2 | 4.3e-2 |

### Balayage de niveau — révèle le hard-clip TX

Sur **32QAM 750 Bd** :

| Niveau | BER |
|---|---|
| 0 dB | 7.2e-3 |
| -3 dB | 2.1e-3 |
| **-6 dB** | **4.7e-5** |
| -10 dB | 3.8e-4 |
| -15 dB | 3.4e-3 |

Facteur **150 d'amélioration** entre 0 dB et -6 dB → le modulateur FM
écrête les pics du signal à fort niveau. Le simulateur a été mis à jour
avec un paramètre `tx_hard_clip` (défaut 0.55) pour modéliser cet effet.

### Sweet spot OTA (après niveau optimal -6 dB)

| Modulation | Rs | BER | Débit brut | FEC | Débit net |
|---|---|---|---|---|---|
| **32QAM** | **750 Bd** | **4.7e-5** | **3.75 kb/s** | 3/4 | **~3 kb/s** |
| 16QAM | 750 Bd | 0 | 3.00 kb/s | 3/4 | 2.25 kb/s |
| 8PSK | 750 Bd | 0 | 2.25 kb/s | minimale | 2.1 kb/s |

Le **32QAM 750 Bd à -6 dB** est le nouveau meilleur compromis, dépassant
le 16QAM trouvé en simulation pure. Au bon niveau d'entrée, on obtient
**~3 kb/s net en broadcast** sur un canal NBFM voice.

### Limite canal (non liée au TX)

**16QAM 1000 Bd** : BER reste ~1e-2 indépendamment du niveau → limite
imposée par la phase non-linéaire du canal (40° std), pas par le TX.
Poussera plus loin nécessite un égaliseur.

## Reproduire

```bash
# Simulation :
python study/modem_ber_bench.py

# OTA :
python study/generate_ota_test_wav.py
# (jouer les WAV sur TX, enregistrer RX)
python study/analyse_ota_recording.py \
    results/rec_part02.wav \
    --timelines results/ota_test_part02_levelsweep02.json
```

Voir le [rapport HTML](../rapport_modem.html) pour les graphiques.
