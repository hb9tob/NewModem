# Modem Design

Choix de conception du modem NBFM broadcast.

## Contraintes

- **Point-multipoint** (pas d'ARQ) → FEC forte, pilotes permanents
- **Resynchronisation rapide** (QRM, nouveau receiver) → pilotes continus + préambule
- **Ouvert** (pas de VARA) → réimplémentation nécessaire
- **PAPR faible** → survit au limiteur voice → **single-carrier** plutôt qu'OFDM

## Single-carrier vs OFDM

| Critère | Single-carrier | OFDM |
|---|---|---|
| PAPR | ~3 dB | 10-12 dB |
| Passe limiteur voice | ✓ | ✗ (distorsion) |
| Synchro | simple (Gardner+Costas) | complexe (CP, pilotes FFT) |
| Égalisation | DFE/LMS courts | par porteuse, naturel |
| Résistance dérive | excellente | bonne (pilotes OFDM) |

→ Pour ce projet, **single-carrier** est le bon choix, confirmé par le fait
que HSMODEM (DJ0ABR) passe bien sur canal FM alors que HamDRM (OFDM) donne
de mauvais résultats.

## Placement des pilotes

### Étude en FM vs SSB

HSMODEM place le pilote au centre du signal data (hérité du SSB QO-100).
En FM, c'est **sous-optimal** à cause du noise shaping triangulaire et
des filtres de bord du transceiver.

### Résultats du banc

Banc : 8PSK 1500 Bd centré 1100 Hz + pilote CW -6 dB/data à différentes
fréquences. SNR pilote mesuré en bande étroite (1 Hz) après canal simulé.

| Position pilote | SNR (dB) | Phase σ (°) |
|---|---|---|
| 400 Hz (bord bas) | **33.1** | 4.5 |
| 600 Hz | 31.1 | 5.4 |
| 1000 Hz (centre data) | 30.6 | 5.6 |
| 1400 Hz | 30.7 | 6.1 |
| 1800 Hz (bord haut) | **32.9** | 4.4 |

→ **Bords de bande gagnent 2-3 dB** (hors spectre data), et 400 Hz profite du
noise shaping FM plus faible en bas.

## Pilote unique vs dual pilotes

| Scénario | SNR/pilote | Info dérive | Info group delay |
|---|---|---|---|
| Mono 400 Hz | **21.4 dB** | oui | non |
| Dual 400/1800 Hz | 18.4/18.9 dB | oui | **oui (+0.016 ms/s)** |

Le dual coûte -3 dB (partage du budget) mais apporte :

- **Phase commune** (moyenne) → correction de la porteuse et dérive
- **Phase différentielle** (delta) → **group delay** → égaliseur
- Résilience à un QRM qui détruit un côté du spectre

Sur ton canal, la phase différentielle a mesuré une dérive de +0.016 ms/s =
**-16 ppm**, exactement ce qu'on avait injecté dans la simulation → le
tracking est exact.

## Choix retenu

- Modulation : **8PSK / 16QAM / 32QAM** (testées)
- Shaping : **RRC α=0.25**
- Symbol rate : **1000 Bd** (max avant collision pilote-data)
- Pilotes : **2 CW à 400 et 1800 Hz, -6 dB/data**
- Data center : **1100 Hz**

**Débit brut atteignable** (sans FEC) :

| Modulation | Rs | Débit brut | Avec LDPC 3/4 | Net |
|---|---|---|---|---|
| 8PSK | 1000 Bd | 3.0 kb/s | 3/4 | 2.25 kb/s |
| 16QAM | 1000 Bd | 4.0 kb/s | 3/4 | **3.0 kb/s** |
| 32QAM | 1000 Bd | 5.0 kb/s | 2/3 | **3.33 kb/s** |

Voir [BER Benchmarks](BER-Benchmarks) pour les courbes.

## Pistes pour pousser au-delà de 1000 Bd

1. **α RRC réduit à 0.1** → jusqu'à ~1270 Bd sans toucher les pilotes
2. **Pilotes TDM** → pas d'interférence spectrale, Rs jusqu'à 1500 Bd
3. **Soustraction pilote au RX** → permet pilotes dans le spectre data
4. **Égaliseur adaptatif** LMS/DFE pour compenser la phase non-linéaire
   40° et les interférences résiduelles
5. **FEC LDPC short frames** (DVB-S2X VL-SNR) → gain sensibilité 5-7 dB
