# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Language convention

**All new code, comments, doc-strings, log messages, identifiers, commit
messages, and PR descriptions must be in English** from 2026-04-29 onwards.
Existing French comments are being migrated incrementally; do not introduce
new French ones, even when editing a file that still has French legacy.

Exception — **user-facing UI strings stay in French** (button labels, hints,
error messages shown in the GUI, capture dialogs, etc.) because the target
audience is French-speaking radio amateurs. When in doubt: if the string is
read by an end user inside the app, French is fine; everywhere else, English.

## Project

Conception d'un modem audio pour transmission d'images sur canal radio amateur NBFM (Narrowband FM). Les transceivers appliquent pré-emphase à l'émission et dé-emphase à la réception.

## Environment

- **Python** : radioconda (Python 3.10) installé dans `C:\Users\tous\radioconda\`
- **GNU Radio** : 3.10.4 via radioconda — utiliser `/c/Users/tous/radioconda/python.exe` pour exécuter les scripts
- **Bibliothèques** : numpy 1.23, scipy 1.9, matplotlib 3.6

## Commands

```bash
# Exécuter un script Python avec GNU Radio
/c/Users/tous/radioconda/python.exe study/nbfm_channel_response.py

# Ouvrir un flowgraph dans GNU Radio Companion
/c/Users/tous/radioconda/Scripts/gnuradio-companion.exe study/nbfm_channel_response.grc
```

## Architecture

```
study/          # Scripts d'étude et caractérisation du canal
results/        # Données CSV et graphiques générés
```

## Canal NBFM — paramètres de référence (défauts GNU Radio)

- Déviation FM : ±5 kHz (défaut `analog.nbfm_tx`)
- Pré-emphase/dé-emphase : 75 µs (défaut GNU Radio)
- Bande audio utile mesurée (-3 dB) : ~100 Hz – 2600 Hz
- Coupure raide au-delà de 2700 Hz (filtre audio du bloc `nbfm_rx`)
- Échantillonnage audio : 48 kHz, IF : 480 kHz
- Bruit modélisé en post-démodulation (audio blanc), SNR cible : 20 dB
