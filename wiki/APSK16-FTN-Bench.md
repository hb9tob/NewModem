# 16-APSK + RRC agressif (FTN) sur canal NBFM

Banc de simulation evaluant une constellation **16-APSK (4,12) DVB-S2** avec
RRC agressif (β ∈ {0.15, 0.20, 0.25}) et optionnellement FTN (Faster Than
Nyquist, τ < 1), pour pousser au-dela du plafond des modems 16-QAM/32-QAM
actuels (~1000 Bd) sur les relais NBFM amateur.

Voir le [rapport HTML complet](https://hb9tob.github.io/NewModem/rapport_apsk16_ftn.html) pour les graphiques, heatmaps et la methodologie detaillee.

## Architecture recepteur

Alignee sur la cible OTA (FSE + DD-PLL + LLR → LDPC LNMS WiMAX 2304) :

```
audio 48 kHz → downmix 1100 Hz → RRC matched (β)
            → sync grossiere (correlation preambule post-MF)
            → decimation entiere d_fse = GCD(SPS, pitch)
            → FSE FFE T/2 (LMS, 17 taps a τ=1, 41 taps a τ=0.9)
              + DFE T-spaced (auto-desactive pour FTN)
            → DD-PLL 2e ordre
            → soft demapper max-log LLR
            → [LDPC LNMS WiMAX 2304 externe]
```

Regles "N entier" strictes : SPS = 48000/Rs entier, pitch = τ·SPS entier,
aucun resampling fractionnaire.

## Blocs standards utilises

| Bloc | Reference |
|------|-----------|
| 16-APSK (4,12) γ=2.85 | DVB-S2 ETSI EN 302 307-1 §5.4.3 |
| RRC Nyquist | Proakis |
| FTN (τ<1) | Mazo 1975, Anderson & Rusek 2013 |
| FSE T/2 LMS | Gitlin-Weinstein |
| DFE LMS | Proakis ch.10 |
| DD-PLL APSK | Meyr/Moeneclaey/Fechtel ch.8 |
| Max-log LLR | Viterbi 1998 |
| GMI BICM | Alvarado et al., IEEE TIT 2008 |
| LDPC WiMAX N=2304 | IEEE 802.16e-2005 |

## Resultats (sweep 180 points, N=1000 sym)

### Sweet spots

| Rs (Bd) | SPS | β | τ | BW (Hz) | Debit uncoded | GMI | BER@if_noise=0 |
|---------|-----|----|-----|---------|---------------|-----|----------------|
| **1500** | 32 | 0.20 | 1.000 | 1800 | 6000 bit/s | 4.00 | 0 |
| **1500** | 32 | 0.25 | 0.938 | 1920 | 6395 bit/s (FTN) | 3.98 | 0.001 |
| 1411.76 | 34 | 0.25 | 1.000 | 1694 | 5647 bit/s | 4.00 | 0 |
| 1200 | 40 | 0.20 | 1.000 | 1440 | 4800 bit/s | 4.00 | 0 |

### Points non-viables (echec confirme)

| Rs (Bd) | Raison |
|---------|--------|
| 1600 τ=1.0 | BW=1920 Hz touche LPF post-demod 2000 Hz → GMI ~2.7 |
| 2000 | BW=2400 Hz hors bande NBFM |
| τ ≤ 0.88 | Cliff FTN, BER>10% sans BCJR (prevu litterature) |

## Decouvertes physiques

1. **Rs=1500 (SPS=32) est le nouveau sweet spot** : +25% de debit vs Rs=1200
   sans perte de robustesse. BW=1800 Hz reste dans le plateau NBFM.
2. **DFE nuit au FTN** (error propagation sur ISI pre-curseur). Le code
   l'auto-desactive quand τ<1.
3. **Genou FTN a τ ≈ 0.90** avec FSE FFE-only, conforme a la theorie
   Anderson/Rusek. τ=0.94 marche, τ=0.85 ne marche pas.
4. **16-APSK sur canal AWGN pur est ~1 dB moins bon que 16-QAM** (dmin plus
   petit). L'avantage APSK sur NBFM viendra surtout du PAPR plus faible
   et de la robustesse au clip TX, a valider OTA.

## Candidats OTA prioritaires

- **Debit max** : `Rs=1500, β=0.20, τ=1.0` → 6000 bit/s uncoded, 4500 bit/s
  avec LDPC rate 3/4.
- **Robustesse max** : `Rs=1411, β=0.25, τ=1.0` → 5647 bit/s uncoded, tolere
  if_noise=0.5.
- **FTN modere** : `Rs=1500, β=0.25, τ=0.938` → 6395 bit/s uncoded, BER 0.001.

## Etude limites de bruit

Balayage fin `if_noise ∈ [0, 1.5]` sur les 5 top candidats, 2000 sym/point.

**SNR sortie FSE @ if_noise = 0** (plancher bruit hors IF) :

| Candidat | SNR-out |
|----------|---------|
| Rs=1200 β=0.20 τ=1.0 | 28.4 dB |
| Rs=1411 β=0.25 τ=1.0 | 25.3 dB |
| Rs=1500 β=0.20 τ=1.0 | 20.9 dB |
| Rs=1500 β=0.25 τ=1.0 | 19.4 dB |
| Rs=1500 β=0.25 τ=0.938 FTN | 15.3 dB |

**Decouverte : effet de seuil FM (threshold effect)**. Cliff brutal entre
`if_noise=0.4` (BER<0.01) et `if_noise=0.5` (BER>0.1), identique pour tous
les candidats. C'est le demodulateur FM qui bascule en regime click-noise
sous IF SNR ~10 dB, pas le modem.

**Implications OTA** :
- Marge modem >10 dB pour tous les candidats avant le FM threshold.
- Le bottleneck est la sensibilite RF, pas la modem.
- Critere OTA : si le signal est bien recu (S7+), marge confortable.
  Si signal a la limite (S1-2), rien ne marchera.

Voir les plots : `results/apsk16_ftn/limit_study/{ber,gmi,snr}_vs_noise.png`
et `ber_vs_snr.png` dans le [rapport HTML](https://hb9tob.github.io/NewModem/rapport_apsk16_ftn.html).

Script : `study/apsk16_ftn_limit_study.py`.

## Pistes parkees

- **FTN τ ≤ 0.88** : necessite BCJR (non aligne avec architecture LNMS-only).
- **Scan γ APSK** : γ=2.85 fixe ici, autres valeurs DVB-S2 possibles phase 2.
- **Timing tracking pilot-aided actif** : diagnostique seulement,
  compensation non necessaire sur paquets <5000 symboles a drift 16 ppm.

## Reproduction

Source : `study/modem_apsk16_ftn_bench.py`.

```bash
# Tests unitaires (rapide)
/c/Users/tous/radioconda/python.exe study/modem_apsk16_ftn_bench.py --test

# Sweep rapide (18 points, ~2 min)
/c/Users/tous/radioconda/python.exe study/modem_apsk16_ftn_bench.py --sweep --quick

# Sweep complet (180 points, ~15-20 min)
/c/Users/tous/radioconda/python.exe study/modem_apsk16_ftn_bench.py --sweep --n-symbols 1000
```

Sorties : `results/apsk16_ftn/{sweep.csv, heatmaps/, recommendation.md, llr_dumps/}`.
