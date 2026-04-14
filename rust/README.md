# nbfm-modem (Rust)

Implementation Rust du modem NBFM single-carrier valide en simulation +
OTA dans `study/`.

## Architecture

```
rust/
  modem-core/    # DSP : constellation, RRC, framing TDM pilots, modulateur
  tx-cli/        # CLI nbfm-tx pour transmission
  (rx-cli/)      # CLI nbfm-rx (a venir)
  (wasm/)        # bindings WebAssembly (a venir)
  (gui/)         # interface web (a venir)
```

## Etat

**v0.1** : TX en ligne de commande sans FEC.

- [x] Workspace Cargo, modem-core
- [x] Constellations 8PSK Gray / 16QAM / 32QAM cross
- [x] Filtres RRC, upsampling, convolution
- [x] Preambule QPSK + insertion pilotes TDM
- [x] Modulateur passband audio 48 kHz
- [x] CLI tx-cli avec options : mode, device, serial PTT, VOX
- [x] Sortie audio cpal cross-platform (Win/Linux)
- [x] PTT serialport DTR/RTS
- [ ] LDPC encoder (WiMAX 802.16, 1/2 et 3/4)
- [ ] RS outer FEC (4 niveaux dont aucun)
- [ ] Compression AVIF d'images
- [ ] Header de frame avec checksum + numero
- [ ] RX (timing recovery, demod, LDPC decode)
- [ ] Bindings WASM
- [ ] GUI web

## Build

```bash
cd rust
cargo build --release
```

Binaire dans `rust/target/release/nbfm-tx[.exe]`.

## Usage

```bash
# Lister les peripheriques audio
nbfm-tx --list-devices

# Lister les ports serie pour PTT
nbfm-tx --list-serial

# Envoyer un fichier au mode rapide (defaut), sans PTT
nbfm-tx --file image.bin

# Mode robuste, carte specifique, PTT via DTR sur COM3
nbfm-tx --file image.bin --mode 8PSK-1/2-500 \
        --device "Sound Blaster" \
        --serial COM3 --ptt-line dtr \
        --vox-duration 0.5
```

## Modes supportes

| Mode | Net bps | Plage SNR | Usage |
|---|---|---|---|
| `32QAM-3/4-1200` | 4235 | >=22 dB | rapide |
| `16QAM-3/4-1500` | 4235 | >=22 dB | rapide |
| `16QAM-1/2-1600` | 3012 | 13-22 dB | medium |
| `8PSK-1/2-1500` | 2118 | 9-13 dB | robuste |
| `8PSK-1/2-500` | 706 | 4-9 dB | survie |

(Note : v0.1 ne fait PAS encore le LDPC — les debits ne tiennent pas compte de la
correction d'erreur. A activer en v0.2.)

## Plate-formes

- **Windows** : compile et tourne. Audio via WASAPI (cpal).
- **Debian 12/13** : devrait fonctionner (cpal supporte ALSA/PulseAudio).
  Tester apres premier commit.
- Autres : bonus.
