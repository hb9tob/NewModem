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

## Reproduire

```bash
python study/modem_ber_bench.py
```

Produit `results/modem_ber_vs_rs.png` et `results/modem_constellations.png`.

Voir le [rapport HTML](../rapport_modem.html) pour les graphiques.
