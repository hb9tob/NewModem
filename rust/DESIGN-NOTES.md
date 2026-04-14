# Design notes - reprise

État au moment de la pause.

## Ce qui est fait (commit `fd0de4e` sur main)

### Rust v0.2

- Workspace `rust/modem-core` + `rust/tx-cli`
- Constellations 8PSK Gray / 16QAM / 32QAM cross
- RRC pulse shaping
- Préambule QPSK + pilotes TDM (32 data + 2 pilotes)
- Modulateur passband audio 48 kHz
- **LDPC encoder WiMAX 802.16** (rate 1/2 et 3/4)
  - Matrices parity exportées de commpy (`export_ldpc.py`)
  - Stockées en binaire compact (`src/ldpc_data/*.bin`)
  - Encodage systématique : `cw = [info | (info × P) mod 2]`
- CLI `nbfm-tx` :
  - Sortie audio cpal (Win/Linux)
  - PTT serialport DTR/RTS
  - VOX preamble (carrier 1100 Hz)
- 13/13 tests unitaires OK

## Ce qui reste à faire

### 1. Header de frame (à coder)

**Structure fixe 16 octets, NON-RS-encodé, juste CRC16 :**

| Offset | Taille | Champ |
|---|---|---|
| 0 | 2 | magic `0xCAFE` |
| 2 | 1 | version |
| 3 | 1 | mode (code modem pour RX) |
| 4 | 1 | rs_level (0..3) |
| 5 | 2 | n_data_blocks (K) |
| 7 | 2 | n_parity_blocks (M) |
| 9 | 2 | frame_id |
| 11 | 1 | flags (bit0=last frame) |
| 12 | 2 | reserved |
| 14 | 2 | header_crc16 (CRC16-CCITT sur octets 0..13) |

### 2. Outer FEC : Reed-Solomon en mode erasure (à coder)

**Décision clé** : RS opère au **niveau du bloc**, pas du byte. C'est un
*erasure code* type RAID-6 (crate `reed-solomon-erasure`), pas un RS
classique de correction d'erreur.

**Pourquoi** : LDPC corrige les erreurs aléatoires bit-à-bit. Chaque
codeword soit décode cleanly soit échoue (position d'échec connue).
RS-erasure recovere les blocs perdus, on a 2x plus de capacité qu'en
mode correction.

**Bloc unitaire** : 90 octets (1 LDPC codeword info, identique pour
rate 1/2 et 3/4).

**Détection bloc cassé** : CRC32 par bloc (4 octets / 90 = 4.4 %
overhead) + signal de convergence du décodeur LDPC.

**4 niveaux RS** (M parity / K data) :

| Level | M/K | Tolérance | Usage |
|---|---|---|---|
| 0 | aucun | 0 | canal parfait |
| 1 | ~6 % | 1 bloc / 16 | léger QRM |
| 2 | ~12 % | 2 blocs / 16 | QRM modéré |
| 3 | ~25 % | 4 blocs / 16 | QRM sévère, fading |

**Filename** : dans le bloc 0 du body avec marqueur de début.

### 3. Compression image AVIF (à coder)

Crate `image` + `ravif` ou `image::codecs::avif`. Permet `nbfm-tx
--file image.png` qui re-compresse en AVIF avant envoi.

### 4. RX (à coder)

- Détection préambule par corrélation
- Estimation phase via pilotes TDM
- Match RRC, slicing
- Soft demod LLR
- LDPC decode (BP min-sum)
- RS erasure decode si rs_level > 0
- Extract filename + payload, écrire fichier

### 5. WASM bindings (à coder)

Compiler modem-core en wasm32-unknown-unknown, exposer modulate_bytes()
et future demodulate() via wasm-bindgen.

### 6. GUI navigateur (à coder)

HTML/JS/wasm. Utilise WebAudio API pour TX et getUserMedia pour RX.

## Ordre proposé de reprise

1. Header + framing (1-2 jours)
2. RS erasure encoder + intégration TX (1-2 jours)
3. Compression AVIF (0.5 jour)
4. RX complet (3-5 jours, le plus gros morceau)
5. WASM + GUI (2-3 jours)

## Liens utiles

- Wiki : https://github.com/hb9tob/NewModem/wiki
- Sweep SNR : https://github.com/hb9tob/NewModem/wiki/BER-Benchmarks
- Improvements TODO : https://github.com/hb9tob/NewModem/wiki/Improvements-TODO
