# NewModem

[Français](#français) — [English](#english)

---

## Français

Modem audio pour transmission d'images via relais NBFM radio amateur.
Modem single-carrier broadcast (point-multipoint, sans ARQ) visant un débit
de 3-4 kb/s net dans le plateau audio NBFM.

### Canal

Les transceivers NBFM amateurs appliquent pré-emphase à l'émission et
dé-emphase à la réception. Le canal a été caractérisé en mode voice et
mode data — voir [rapport canal](rapport_canal_nbfm.html).

| Paramètre | Valeur |
|---|---|
| Bande utile (-3 dB) | 300 – 2200 Hz |
| SNR audio mode data | ~30 dB |
| Dérive horloge soundcard | -16 ppm (mesurée) |
| Phase non-linéaire | ~40° std dans la bande |
| Mode voice | limiteur actif (compression ~1 dB) |

### Modem

Design single-carrier inspiré du modem QO-100 d'EA4GPZ (HSMODEM), avec
LDPC WiMAX 2304 (LNMS) et chaîne RX FSE T/2 + DD-PLL + soft demap LLR.

Sept profils sélectionnables (`rust/modem-core/src/profile.rs`) — cinq
stables auto-détectés et deux expérimentaux qui demandent un mode forcé
côté RX :

| Profil | Constellation | Rs (Bd) | β | τ | LDPC | Net brut |
|---|---|---|---|---|---|---|
| ULTRA  | QPSK     | 500  | 0.25 | 1.0   | 1/2 | ~444 bps |
| ROBUST | QPSK     | 1000 | 0.25 | 1.0   | 1/2 | ~941 bps |
| NORMAL | 8-PSK    | 1500 | 0.20 | 1.0   | 1/2 | ~2 117 bps |
| HIGH   | 16-APSK  | 1500 | 0.20 | 1.0   | 3/4 | ~4 235 bps |
| MEGA   | 16-APSK  | 1500 | 0.20 | 30/32 | 3/4 | ~3 971 bps |
| **HIGH+** ⚠ | **32-APSK DVB-S2** | 1500 | 0.20 | 1.0 | 3/4 | **~5 294 bps** |
| **FAST** ⚠  | 16-APSK | **1714** (sps=28) | **0.15** | 1.0 | 3/4 | **~4 840 bps** |

Le sweet spot **Rs=1500 Bd, β=0.20** tient dans la BW utile NBFM ; MEGA
ajoute du FTN (τ<1) pour pousser le débit. Voir [rapport modem SC](rapport_modem.html)
et [rapport 16-APSK + FTN](rapport_apsk16_ftn.html).

⚠ **Profils expérimentaux HIGH+ et FAST** — validés OTA à 100 % sur
relais NBFM dans nos tests. Hors auto-détection : le pair RX doit
activer "Forcer un profil" dans l'onglet RX et choisir le même mode.
HIGH+ utilise la constellation 32-APSK DVB-S2 (rayons γ1=2.84, γ2=5.27
pour LDPC 3/4 — table 10 ETSI EN 302 307-1). FAST resserre β et pousse
Rs pour gagner ~14 % vs HIGH sans changer la constellation.

### Simulateur de canal

Simulateur basé sur les blocs GNU Radio `analog.nbfm_tx` / `nbfm_rx` :

- bruit gaussien complexe injecté à l'IF (entre pré-emphase et de-emphase) ;
- HPF sub-audio 300 Hz (filtre CTCSS) et LPF post-démod 2400 Hz ;
- dérive d'horloge soundcard paramétrable (statique + thermique) ;
- délai initial TX→RX aléatoire 2–5 s (force la resynchro).

```bash
python study/nbfm_channel_sim.py input.wav output.wav \
    --if-noise 0.165 --drift-ppm -16 \
    --thermal-ppm 5 --thermal-period 180
```

### Structure du dépôt

```
study/                  Scripts d'étude, simulateur, bancs de test modem
results/                WAV de mesure, CSV, graphiques générés
wiki/                   Pages du wiki GitHub
rust/modem-core/        Cœur modem (constellations, LDPC, sync, RX pipeline)
rust/modem-cli/         Binaire `nbfm-modem` (TX/RX via WAV)
rust/modem-gui/         Interface Tauri (RX live, TX, Sessions, Canal)
rust/newmodem-collector/ Serveur web qui collecte les sondages de canal
dist/portable/          Sorties des scripts portables (zip Windows / tar.gz Linux)
rapport_*.html          Rapports HTML avec graphiques
```

### Build — Linux (Debian 12 / Ubuntu 22.04+)

**Dépendances système :**

```bash
sudo apt update
sudo apt install -y \
    build-essential curl wget file pkg-config git \
    libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
    libayatana-appindicator3-dev librsvg2-dev \
    libasound2-dev
```

`libwebkit2gtk-4.1-dev` est requis par Tauri 2 (pas `-4.0`).
`libasound2-dev` est requis par `cpal` (backend ALSA).

**Toolchain Rust** (Debian 12 ship 1.63, trop vieux — utiliser rustup) :

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version   # >= 1.77
cargo install tauri-cli --version "^2" --locked
```

**Build CLI** (binaire `nbfm-modem`, TX/RX via WAV) :

```bash
cd rust
cargo build -p modem-cli --release
./target/release/nbfm-modem --help
```

**Build GUI** (binaire Tauri) :

```bash
cd rust/modem-gui/src-tauri
cargo tauri build                    # bundle complet (.deb + AppImage)
cargo tauri build --bundles deb      # .deb uniquement (plus rapide)
ls ../../target/release/bundle/deb/  # nbfm-modem-gui_*_amd64.deb
```

Détails et dépannage : [`rust/modem-gui/BUILD_DEBIAN.md`](rust/modem-gui/BUILD_DEBIAN.md).

**Archive portable Linux** (CLI + GUI + marqueur portable) :

```bash
./make-portable.sh                   # tag = git describe
./make-portable.sh v0.1.0-test       # tag explicite
./make-portable.sh "" --skip-build   # saute les cargo build
```

### Build — Windows 10/11 (MSVC)

**Pré-requis :**

- Visual Studio 2022 Build Tools avec workload *Desktop development with C++*
  (fournit `cl.exe` et le linker MSVC).
- Microsoft Edge WebView2 Runtime (pré-installé sur Windows 11).
- [rustup](https://rustup.rs/) avec la toolchain `stable-x86_64-pc-windows-msvc`.
- Git for Windows.

```powershell
rustup default stable-x86_64-pc-windows-msvc
rustc --version
cargo install tauri-cli --version "^2" --locked
```

**Build CLI :**

```powershell
cd rust
cargo build -p modem-cli --release
.\target\release\nbfm-modem.exe --help
```

**Build GUI :**

```powershell
cd rust\modem-gui\src-tauri
cargo tauri build
# Installateur MSI/NSIS : rust\target\release\bundle\msi\ ou \nsis\
```

**Archive portable Windows** (zip auto-suffisant avec CLI + GUI + marqueur
portable, settings/captures/sessions stockés à côté du `.exe`) :

```powershell
.\make-portable.ps1                    # tag = git describe
.\make-portable.ps1 -Tag v0.1.0-test
.\make-portable.ps1 -SkipBuild
```

### Environnement Python (scripts d'étude)

Les bancs `study/*.py` utilisent radioconda :

- Python 3.10 (`C:\Users\tous\radioconda\` sur la machine de dev)
- GNU Radio 3.10.4
- numpy 1.23, scipy 1.9, matplotlib 3.6

```bash
/c/Users/tous/radioconda/python.exe study/<script>.py
```

### Scripts principaux

| Script | Rôle |
|---|---|
| `nbfm_channel_sim.py` | Simulateur de canal NBFM (GNU Radio + drift) |
| `generate_level_sweep_wav.py` | WAV multitone balayage de niveau |
| `analyse_level_sweep.py` | Analyse gain/BW par niveau |
| `analyse_phase.py` | Analyse réponse en phase |
| `validate_simulator.py` | Validation sim vs enregistrements réels |
| `pilot_placement_bench.py` | Optimisation position pilote CW |
| `dual_pilot_bench.py` | Comparaison mono vs dual pilotes |
| `modem_ber_bench.py` | Banc BER multi-constellation vs symbol rate |
| `modem_apsk16_ftn_bench.py` | Banc 16-APSK + FTN |
| `modem_ldpc_ber_bench.py` | Banc LDPC LNMS WiMAX 2304 |

---

## English

Audio modem for image transmission over amateur-radio NBFM repeaters.
Single-carrier broadcast modem (point-to-multipoint, no ARQ) targeting a
3-4 kb/s net throughput inside the NBFM audio plateau.

### Channel

Amateur NBFM transceivers apply pre-emphasis on TX and de-emphasis on RX.
The channel was characterised in voice and data mode — see
[channel report](rapport_canal_nbfm.html).

| Parameter | Value |
|---|---|
| Useful bandwidth (-3 dB) | 300 – 2200 Hz |
| Audio SNR (data mode) | ~30 dB |
| Sound-card clock drift | -16 ppm (measured) |
| Non-linear phase | ~40° std across the band |
| Voice mode | active limiter (~1 dB compression) |

### Modem

Single-carrier design inspired by EA4GPZ's QO-100 modem (HSMODEM), with
WiMAX 2304 LDPC (LNMS) and an RX chain built on FSE T/2 + DD-PLL + soft
LLR demap.

Seven selectable profiles (`rust/modem-core/src/profile.rs`) — five
stable ones with auto-detection plus two experimental that require
forced-mode RX:

| Profile | Constellation | Rs (Bd) | β | τ | LDPC | Net |
|---|---|---|---|---|---|---|
| ULTRA  | QPSK     | 500  | 0.25 | 1.0   | 1/2 | ~444 bps |
| ROBUST | QPSK     | 1000 | 0.25 | 1.0   | 1/2 | ~941 bps |
| NORMAL | 8-PSK    | 1500 | 0.20 | 1.0   | 1/2 | ~2,117 bps |
| HIGH   | 16-APSK  | 1500 | 0.20 | 1.0   | 3/4 | ~4,235 bps |
| MEGA   | 16-APSK  | 1500 | 0.20 | 30/32 | 3/4 | ~3,971 bps |
| **HIGH+** ⚠ | **32-APSK DVB-S2** | 1500 | 0.20 | 1.0 | 3/4 | **~5,294 bps** |
| **FAST** ⚠  | 16-APSK | **1714** (sps=28) | **0.15** | 1.0 | 3/4 | **~4,840 bps** |

The **Rs=1500 Bd, β=0.20** sweet spot fits inside the NBFM audio plateau;
MEGA layers FTN (τ<1) on top to push throughput. See
[SC modem report](rapport_modem.html) and [16-APSK + FTN report](rapport_apsk16_ftn.html).

⚠ **Experimental profiles HIGH+ and FAST** — validated OTA at 100 % on
NBFM repeaters in our tests. Excluded from auto-detection: the RX peer
must enable "Forcer un profil" in the RX tab and pick the same mode.
HIGH+ uses the DVB-S2 32-APSK constellation (radii γ1=2.84, γ2=5.27 for
LDPC 3/4 — table 10 in ETSI EN 302 307-1). FAST tightens β and bumps Rs
to gain ~14 % over HIGH without changing the constellation.

### Channel simulator

Simulator built around the GNU Radio `analog.nbfm_tx` / `nbfm_rx` blocks:

- complex Gaussian noise injected at IF (between pre-emphasis and de-emphasis);
- 300 Hz sub-audio HPF (CTCSS filter) and 2400 Hz post-demod LPF;
- configurable sound-card clock drift (static + thermal);
- random initial TX→RX delay 2–5 s (forces resync).

```bash
python study/nbfm_channel_sim.py input.wav output.wav \
    --if-noise 0.165 --drift-ppm -16 \
    --thermal-ppm 5 --thermal-period 180
```

### Repository layout

```
study/                  Study scripts, simulator, modem benchmarks
results/                Measurement WAVs, CSVs, generated plots
wiki/                   GitHub wiki pages
rust/modem-core/        Modem core (constellations, LDPC, sync, RX pipeline)
rust/modem-cli/         `nbfm-modem` binary (TX/RX over WAV)
rust/modem-gui/         Tauri GUI (live RX, TX, Sessions, Channel tab)
rust/newmodem-collector/ Web server collecting channel soundings
dist/portable/          Output of the portable scripts (Windows zip / Linux tar.gz)
rapport_*.html          HTML reports with plots
```

### Build — Linux (Debian 12 / Ubuntu 22.04+)

**System dependencies:**

```bash
sudo apt update
sudo apt install -y \
    build-essential curl wget file pkg-config git \
    libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
    libayatana-appindicator3-dev librsvg2-dev \
    libasound2-dev
```

`libwebkit2gtk-4.1-dev` is required by Tauri 2 (not `-4.0`).
`libasound2-dev` is required by `cpal` (ALSA backend).

**Rust toolchain** (Debian 12 ships 1.63, too old — use rustup):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version   # >= 1.77
cargo install tauri-cli --version "^2" --locked
```

**CLI build** (`nbfm-modem` binary, TX/RX over WAV):

```bash
cd rust
cargo build -p modem-cli --release
./target/release/nbfm-modem --help
```

**GUI build** (Tauri binary):

```bash
cd rust/modem-gui/src-tauri
cargo tauri build                    # full bundle (.deb + AppImage)
cargo tauri build --bundles deb      # .deb only (faster)
ls ../../target/release/bundle/deb/  # nbfm-modem-gui_*_amd64.deb
```

Details and troubleshooting: [`rust/modem-gui/BUILD_DEBIAN.md`](rust/modem-gui/BUILD_DEBIAN.md).

**Linux portable archive** (CLI + GUI + portable marker):

```bash
./make-portable.sh                   # tag = git describe
./make-portable.sh v0.1.0-test       # explicit tag
./make-portable.sh "" --skip-build   # skip cargo build
```

### Build — Windows 10/11 (MSVC)

**Prerequisites:**

- Visual Studio 2022 Build Tools with the *Desktop development with C++*
  workload (provides `cl.exe` and the MSVC linker).
- Microsoft Edge WebView2 Runtime (pre-installed on Windows 11).
- [rustup](https://rustup.rs/) with the `stable-x86_64-pc-windows-msvc` toolchain.
- Git for Windows.

```powershell
rustup default stable-x86_64-pc-windows-msvc
rustc --version
cargo install tauri-cli --version "^2" --locked
```

**CLI build:**

```powershell
cd rust
cargo build -p modem-cli --release
.\target\release\nbfm-modem.exe --help
```

**GUI build:**

```powershell
cd rust\modem-gui\src-tauri
cargo tauri build
# MSI/NSIS installer: rust\target\release\bundle\msi\ or \nsis\
```

**Windows portable archive** (self-contained zip with CLI + GUI + portable
marker; settings/captures/sessions stored next to the `.exe`):

```powershell
.\make-portable.ps1                    # tag = git describe
.\make-portable.ps1 -Tag v0.1.0-test
.\make-portable.ps1 -SkipBuild
```

### Python environment (study scripts)

The `study/*.py` benchmarks use radioconda:

- Python 3.10 (`C:\Users\tous\radioconda\` on the dev machine)
- GNU Radio 3.10.4
- numpy 1.23, scipy 1.9, matplotlib 3.6

```bash
/c/Users/tous/radioconda/python.exe study/<script>.py
```

### Main scripts

| Script | Role |
|---|---|
| `nbfm_channel_sim.py` | NBFM channel simulator (GNU Radio + drift) |
| `generate_level_sweep_wav.py` | Multitone level-sweep WAV |
| `analyse_level_sweep.py` | Per-level gain/BW analysis |
| `analyse_phase.py` | Phase response analysis |
| `validate_simulator.py` | Simulator validation against real recordings |
| `pilot_placement_bench.py` | CW pilot placement optimisation |
| `dual_pilot_bench.py` | Single vs dual pilot comparison |
| `modem_ber_bench.py` | Multi-constellation BER vs symbol rate |
| `modem_apsk16_ftn_bench.py` | 16-APSK + FTN benchmark |
| `modem_ldpc_ber_bench.py` | WiMAX 2304 LDPC LNMS benchmark |
