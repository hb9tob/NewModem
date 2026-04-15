# Channel Characterization

Caractérisation du canal NBFM réel sur deux transceivers amateurs, mode voice
et data. Toutes les mesures ont été faites avec un signal multitone (79 tones
100-4000 Hz) à différents niveaux d'entrée.

## Paramètres mesurés

| Paramètre | Valeur |
|---|---|
| Bande utile (-3 dB) | 300 – 2200 Hz |
| Coupure basse | HPF CTCSS à 300 Hz (|65 dB d'atténuation à 100 Hz) |
| Coupure haute | LPF doux à partir de 1500 Hz, -25 dB à 4 kHz |
| SNR audio (mode data) | ~30 dB sur la bande utile |
| Plancher de bruit (gaps) | 0.00113 RMS |
| Dérive d'horloge | **-16 ppm** (mesuré soundcard TX+RX) |
| Phase non-linéaire | **~40° std** dans 300-2500 Hz |
| Group delay | ~4-5 ms, ~1 ms de variation dans la bande |

## Mode voice vs mode data

**Voice** : le limiteur audio du transceiver est actif. Conséquences :

- Compression ~1 dB aux niveaux élevés (0 à -2 dB en entrée)
- **Écrasement de la bande haute** à niveau nominal (BW -3 dB = 1450 Hz
  au lieu de 2150 Hz)
- **Non utilisable** à niveau nominal pour un modem linéaire

**Data** : limiteur désactivé. Réponse **parfaitement linéaire** sur 20 dB
de plage dynamique (gain et BW stables à ±0.1 dB / ±50 Hz).

→ Pour un modem **linéaire**, le mode data est obligatoire. Pour un modem
**capable de survivre au limiteur voice** (broadcast sur relais voix), il
faut un signal à **PAPR faible** (single-carrier, pas OFDM).

## Protocole de mesure

1. Générer un WAV multitone avec balayage de niveau (0 à -20 dB par pas de
   2 dB), structure connue avec pilotes de synchro
2. Jouer le WAV sur l'entrée TX, enregistrer la sortie RX (48 kHz mono 16-bit)
3. Analyser par FFT sur bins exacts (fréquences multiples de 1/durée)

Voir [Scripts Reference](Scripts-Reference) pour les commandes exactes.

## Anomalies documentées

- **Bloc -18 dB en mode voice** : gain anormal (-11.5 dB vs -4.1 dB attendu),
  résidu de phase 89° vs 40°. Probable artefact d'enregistrement (squelch
  momentané, QRM externe). **Exclu** des analyses.
- **Dérive apparente du group delay avec le niveau** en mode data :
  artefact d'horloge soundcard qui accumule sur la durée des blocs,
  pas un effet du canal.

## Rapport complet

Voir le [rapport HTML](https://hb9tob.github.io/NewModem/rapport_canal_nbfm.html) avec tous les graphiques.
