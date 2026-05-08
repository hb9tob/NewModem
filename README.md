# NewModem

[Français](#français) — [English](#english)

---

## Français

Modem audio pour transmission d'images via relais NBFM radio amateur.
Modem single-carrier broadcast (point-multipoint, sans ARQ) couvrant
~444 bps (ULTRA, QPSK lent) à ~6 kb/s net (HIGH++ expérimental, 64-APSK)
dans le plateau audio NBFM. Sweet spot autour de 4-5 kb/s avec
HIGH / HIGH56 / HIGH+, sélectionnable selon les conditions du canal.

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

Dix profils sélectionnables (`rust/modem-core/src/profile.rs`) — six
standards auto-détectés et quatre expérimentaux qui demandent un mode
forcé côté RX :

| Profil | Constellation | Rs (Bd) | β | τ | LDPC | Pilotes | Net |
|---|---|---|---|---|---|---|---|
| ULTRA   | QPSK     | 500  | 0.25 | 1.0   | 1/2 | 16/2 | ~444 bps |
| ROBUST  | QPSK     | 1000 | 0.25 | 1.0   | 1/2 | 32/2 | ~941 bps |
| NORMAL  | 8-PSK    | 1500 | 0.20 | 1.0   | 1/2 | 32/2 | ~2 118 bps |
| HIGH    | 16-APSK  | 1500 | 0.20 | 1.0   | 3/4 | 32/2 | ~4 235 bps |
| HIGH56  | 16-APSK  | 1500 | 0.20 | 1.0   | 5/6 | 32/2 | ~4 706 bps |
| HIGH+   | 32-APSK DVB-S2 | 1500 | 0.20 | 1.0 | 3/4 | 32/2 | ~5 294 bps |
| **MEGA** ⚠   | 16-APSK | 1500 | 0.20 | **30/32** | 3/4 | 32/2 | **~3 971 bps** |
| **FAST** ⚠   | 16-APSK | **1714** (sps=28) | **0.15** | 1.0 | 3/4 | 32/2 | **~4 840 bps** |
| **HIGH+56** ⚠ | 32-APSK | 1500 | 0.20 | 1.0 | **5/6** | 32/2 | **~5 882 bps** |
| **HIGH++** ⚠ | **64-APSK DVB-S2X** (4+12+20+28) | 1500 | 0.20 | 1.0 | 3/4 | 16/2 | **~6 000 bps** |

Le sweet spot **Rs=1500 Bd, β=0.20** tient dans la BW utile NBFM. La
colonne **Pilotes** donne le pattern TDM (data/pilot par groupe) : 32/2
par défaut, 16/2 densifié sur ULTRA (drift sub-Nyquist) et HIGH++
(dmin réduite en 64-APSK). **HIGH56** est le profil par défaut côté GUI.
Voir [rapport modem SC](rapport_modem.html) et
[rapport 16-APSK + FTN](rapport_apsk16_ftn.html).

⚠ **Profils expérimentaux** — hors auto-détection : le pair RX doit
activer "Forcer un profil" dans l'onglet RX et choisir le même mode.
Quatre stratégies pour pousser le débit :

- **MEGA** — 16-APSK avec FTN τ=30/32. Gain marginal sur HIGH au prix
  d'un récepteur plus exigeant ; laissé en expérimental après promotion
  de HIGH+ comme standard.
- **FAST** — resserre β à 0.15 et pousse Rs à 1714 Bd (sps=28 entier
  à 48 kHz) pour gagner ~14 % vs HIGH sans changer la constellation.
- **HIGH+56** — combine la constellation 32-APSK et le LDPC 5/6
  (rendements cumulés). +11 % vs HIGH+ au prix d'environ 0.7 dB de
  marge LDPC en moins.
- **HIGH++** — 64-APSK DVB-S2X (rayons γ1=2.4, γ2=4.3, γ3=7.0, table
  13f EN 302 307-2 V1.4.1). Constellation la plus dense livrée à ce
  jour ; pilotes densifiés 16/2 indispensables pour soutenir le
  tracking de phase à dmin réduite.

HIGH+ a été promu standard après validation OTA répétée sur HB9MM.
Les rayons APSK utilisés : HIGH+ → γ1=2.84, γ2=5.27 (table 10
EN 302 307-1, LDPC 3/4) ; HIGH++ → γ1/γ2/γ3 listés ci-dessus.

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

### Périphériques d'entrée RX

Le modem RX peut être alimenté par trois familles de sources, sélectionnables
dans le menu déroulant *Carte son réception* (la liste regroupe tout) :

1. **Carte son** (chemin par défaut) — `cpal` → ALSA / WASAPI / CoreAudio.
   Branche la sortie ligne du transceiver sur l'entrée micro/ligne. Le
   Squelch et le filtre dé-emphase tournent côté radio.
2. **PlutoSDR** (ADALM-Pluto) — backend `pluto` via libiio. RX *et* TX sur
   un seul USB. Voir [`rust/modem-pluto/README.md`](rust/modem-pluto/README.md).
3. **SDRplay** — RX seul. Devices supportés (détection automatique du
   modèle via le byte `hwVer` du daemon) :

   | Device | Ports antenne | Bias-T | Notch FM/DAB | LNA states (VHF) |
   |---|---|---|---|---|
   | RSPduo | Hi-Z + 50 Ω (tuner A), 50 Ω (tuner B) | tuner B | oui | 10 |
   | RSP1A / RSP1B | 1 SMA | oui | oui | 10 |
   | RSP1 | 1 SMA | non | non | 4 |

   RSP2 / RSPdx / RSPdx-R2 : reconnus mais pas encore programmés (le
   backend rejette à `open()` avec un message clair).

#### Installation de l'API SDRplay (Linux)

L'API SDRplay 3.x est un binaire propriétaire non redistribuable. Procédure
unique par machine :

```bash
# 1. Télécharger l'installeur depuis https://www.sdrplay.com/api/
#    (compte gratuit requis). Choisir l'archi du PC :
#       x86_64 → SDRplay_RSP_API-Linux-3.X.Y.run
#       Pi 5 / ARM64 → SDRplay_RSP_API-Linux-ARM64-3.X.Y.run

# 2. Lancer l'installeur en root, accepter l'EULA :
chmod +x SDRplay_RSP_API-Linux-ARM64-3.15.2.run
sudo ./SDRplay_RSP_API-Linux-ARM64-3.15.2.run

# 3. Activer le service systemd (le daemon doit tourner quand le modem
#    cherche des devices) :
sudo systemctl enable --now sdrplay
systemctl status sdrplay     # doit afficher "active (running)"

# 4. Brancher le device et vérifier qu'il est visible :
lsusb | grep -i sdrplay      # doit lister le RSPduo / RSP1A / etc.
```

Ce que l'installeur dépose :

- `/usr/local/lib/libsdrplay_api.so.3.X` (+ liens symboliques) — la lib FFI
- `/usr/local/include/sdrplay_api*.h` — les en-têtes que `bindgen` lit au build
- `/opt/sdrplay_api/sdrplay_apiService` — le daemon
- `/etc/udev/rules.d/66-sdrplay.rules` — permissions USB pour utilisateur non-root
- Service systemd `sdrplay.service`

**Pannes courantes :**

| Symptôme | Cause / correctif |
|---|---|
| Device absent du dropdown GUI | `systemctl status sdrplay` → pas actif. `sudo systemctl start sdrplay` |
| Daemon présent mais pas de device | `sudo udevadm control --reload && sudo udevadm trigger`, puis débrancher / rebrancher l'USB |
| `Permission denied` sur `/dev/bus/usb` | L'utilisateur n'est pas dans le groupe `plugdev` (rare — l'installeur le configure normalement). `sudo usermod -aG plugdev $USER` puis relogin |
| Build de `modem-sdrplay` échoue (`fatal: sdrplay_api.h not found`) | Headers absents : réinstaller l'API. Si dans un chemin atypique, `export SDRPLAY_API_INCLUDE_DIR=/path/to/include SDRPLAY_API_LIB_DIR=/path/to/lib` avant `cargo build` |
| GUI démarre mais le device disparaît au lancement RX | API ouverte par un autre process (SDRuno, SoapySDRPlay3 daemon). Fermer l'autre client |

L'API SDRplay est requise **à la fois au build et au runtime** : `build.rs`
appelle `bindgen` sur `/usr/local/include/sdrplay_api*.h` et fait paniquer
le compile si les headers manquent (avec un message qui pointe vers la
page d'install). Si tu construis le `.deb` sur une machine, et qu'une
autre l'installe, les deux ont besoin de l'API. Pour court-circuiter le
support SDRplay au build (cas d'un installeur ARM-only sans accès aux
headers), désactiver la feature : `cargo tauri build --no-default-features
--features pluto`.

### ⚠ Carte son sous Windows — vérifier le format à 48 kHz

**Symptôme** : votre carte son (typiquement **SignaLink USB**, ou tout autre
boîtier USB type CM119) apparaît dans la liste *Carte son réception* sans le
tag *✓48k*, et le bouton *Démarrer* reste grisé. La GUI affiche par exemple
`SignaLink USB — 44100–44100 Hz` au lieu de `48000–48000 Hz`.

**Cause** : sur Windows, l'API audio (WASAPI partagé) n'expose pas les
formats réels du matériel — elle ne renvoie que le **format par défaut**
configuré dans les propriétés Windows du périphérique. Si Windows est en
44,1 kHz, l'app voit 44,1 kHz, point. Le CM119 du SignaLink supporte
nativement 48 kHz mais la couche partagée le masque.

**Correctif (à faire une fois par carte son) :**

1. Clic droit sur l'icône haut-parleur (zone de notification) → *Sons*
   (ou : Panneau de configuration → Matériel et audio → Son).
2. Onglet **Enregistrement** → double-clic sur l'entrée du SignaLink
   (souvent `Microphone (USB Audio CODEC)`) → onglet **Avancé** → liste
   *Format par défaut* → choisir **« 2 canaux, 16 bits, 48000 Hz (qualité DVD) »**
   → *Appliquer* / *OK*.
3. Onglet **Lecture** → faire la même chose sur la sortie SignaLink (sinon
   la chaîne TX ressamplera 48 → 44,1 → 48 kHz et abîmera le signal modem).
4. Fermer puis rouvrir le NBFM Modem ; l'entrée doit maintenant afficher
   `48000–48000 Hz ✓48k` et *Démarrer* devient cliquable.

Vrai pour toutes les interfaces USB-audio classe (SignaLink, RIGblaster,
DigiRig, câbles CM108/CM119 isolés, etc.). Une fois fait, le réglage est
persistant — on ne le refait que si on change de carte ou réinstalle Windows.

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

> **Si tu veux le backend SDRplay** (RSPduo / RSP1A / RSP1B / RSP1) :
> installe l'API SDRplay 3.x **avant** le build —
> [procédure complète](#installation-de-lapi-sdrplay-linux). Sinon,
> désactive la feature : `cargo tauri build --no-default-features --features pluto`.

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

### Build — Raspberry Pi 5 (Debian 13 trixie, 7" écran tactile)

Build natif identique au flux Linux ci-dessus, avec deux particularités :

- L'apt-rust de Debian 13 fournit Rust 1.85, **insuffisant** pour la
  crate `raptorq` 2.0.1 qui requiert ≥ 1.88 (`let-chains`,
  `unsigned_is_multiple_of`). Passer par `rustup` :

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  source "$HOME/.cargo/env"
  ```

- Pour l'écran officiel Raspberry Pi 7" (DSI 800×480), un **build kiosk
  dédié** active automatiquement plein écran sans bordure et un layout
  compact via `@media (max-width: 900px)` :

  ```bash
  cd rust/modem-gui/src-tauri
  cargo tauri build --bundles deb --config configs/tauri.pi.conf.json
  ```

  Le `.desktop` produit injecte `Exec=env NBFM_KIOSK=1 …` ; au
  démarrage le binaire Rust détecte cette variable et applique
  `set_fullscreen` + `set_decorations(false)`. Bouton ✕ dans la barre
  d'onglets pour quitter, **Échap** pour basculer plein écran ↔
  fenêtré. Build standard sans `--config` = comportement desktop
  classique. Détails :
  [`rust/modem-gui/src-tauri/configs/README.md`](rust/modem-gui/src-tauri/configs/README.md).

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
Single-carrier broadcast modem (point-to-multipoint, no ARQ) covering
~444 bps (ULTRA, slow QPSK) up to ~6 kb/s net (HIGH++ experimental,
64-APSK) inside the NBFM audio plateau. Sweet spot around 4-5 kb/s with
HIGH / HIGH56 / HIGH+, picked according to channel conditions.

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

Ten selectable profiles (`rust/modem-core/src/profile.rs`) — six
standard ones with auto-detection plus four experimental that require
forced-mode RX:

| Profile | Constellation | Rs (Bd) | β | τ | LDPC | Pilots | Net |
|---|---|---|---|---|---|---|---|
| ULTRA   | QPSK     | 500  | 0.25 | 1.0   | 1/2 | 16/2 | ~444 bps |
| ROBUST  | QPSK     | 1000 | 0.25 | 1.0   | 1/2 | 32/2 | ~941 bps |
| NORMAL  | 8-PSK    | 1500 | 0.20 | 1.0   | 1/2 | 32/2 | ~2,118 bps |
| HIGH    | 16-APSK  | 1500 | 0.20 | 1.0   | 3/4 | 32/2 | ~4,235 bps |
| HIGH56  | 16-APSK  | 1500 | 0.20 | 1.0   | 5/6 | 32/2 | ~4,706 bps |
| HIGH+   | 32-APSK DVB-S2 | 1500 | 0.20 | 1.0 | 3/4 | 32/2 | ~5,294 bps |
| **MEGA** ⚠   | 16-APSK | 1500 | 0.20 | **30/32** | 3/4 | 32/2 | **~3,971 bps** |
| **FAST** ⚠   | 16-APSK | **1714** (sps=28) | **0.15** | 1.0 | 3/4 | 32/2 | **~4,840 bps** |
| **HIGH+56** ⚠ | 32-APSK | 1500 | 0.20 | 1.0 | **5/6** | 32/2 | **~5,882 bps** |
| **HIGH++** ⚠ | **64-APSK DVB-S2X** (4+12+20+28) | 1500 | 0.20 | 1.0 | 3/4 | 16/2 | **~6,000 bps** |

The **Rs=1500 Bd, β=0.20** sweet spot fits inside the NBFM audio plateau.
The **Pilots** column gives the TDM pattern (data/pilot per group):
32/2 by default, 16/2 densified on ULTRA (sub-Nyquist drift) and HIGH++
(reduced dmin in 64-APSK). **HIGH56** is the GUI default. See
[SC modem report](rapport_modem.html) and
[16-APSK + FTN report](rapport_apsk16_ftn.html).

⚠ **Experimental profiles** — excluded from auto-detection: the RX peer
must enable "Forcer un profil" in the RX tab and pick the same mode.
Four strategies to push throughput:

- **MEGA** — 16-APSK with FTN τ=30/32. Marginal gain over HIGH at the
  cost of a more demanding receiver; kept experimental after HIGH+ was
  promoted to standard.
- **FAST** — tightens β to 0.15 and bumps Rs to 1714 Bd (integer
  sps=28 at 48 kHz) to gain ~14 % over HIGH without changing the
  constellation.
- **HIGH+56** — combines the 32-APSK constellation and LDPC 5/6
  (compounded throughput). +11 % over HIGH+ at the cost of about
  0.7 dB less LDPC margin.
- **HIGH++** — 64-APSK DVB-S2X (radii γ1=2.4, γ2=4.3, γ3=7.0 from
  table 13f in EN 302 307-2 V1.4.1). Densest constellation shipped so
  far; densified pilots 16/2 are required to support phase tracking at
  reduced dmin.

HIGH+ was promoted to standard after repeated OTA validation on HB9MM.
APSK radii used: HIGH+ → γ1=2.84, γ2=5.27 (table 10 EN 302 307-1, LDPC
3/4); HIGH++ → γ1/γ2/γ3 listed above.

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

### RX input devices

The modem RX can be driven from three families of sources, all listed
together in the *RX sound card* dropdown:

1. **Sound card** (default path) — `cpal` → ALSA / WASAPI / CoreAudio.
   Wire the transceiver's line-out into the host's mic/line-in. Squelch
   and de-emphasis run on the radio side.
2. **PlutoSDR** (ADALM-Pluto) — `pluto` backend via libiio. RX *and* TX
   over one USB. See [`rust/modem-pluto/README.md`](rust/modem-pluto/README.md).
3. **SDRplay** — RX-only. Supported devices (auto-detected from the
   daemon's `hwVer` byte) :

   | Device | Antenna ports | Bias-T | FM/DAB notch | LNA states (VHF) |
   |---|---|---|---|---|
   | RSPduo | Hi-Z + 50 Ω (tuner A), 50 Ω (tuner B) | tuner B | yes | 10 |
   | RSP1A / RSP1B | 1 SMA | yes | yes | 10 |
   | RSP1 | 1 SMA | no | no | 4 |

   RSP2 / RSPdx / RSPdx-R2: detected but not yet programmed — the
   backend rejects at `open()` with a clear error.

#### SDRplay API install (Linux)

The SDRplay API 3.x is a proprietary, non-redistributable binary blob.
One-time install per machine:

```bash
# 1. Download the installer from https://www.sdrplay.com/api/
#    (free account required). Pick the right arch:
#       x86_64 → SDRplay_RSP_API-Linux-3.X.Y.run
#       Pi 5 / ARM64 → SDRplay_RSP_API-Linux-ARM64-3.X.Y.run

# 2. Run the installer as root, accept the EULA:
chmod +x SDRplay_RSP_API-Linux-ARM64-3.15.2.run
sudo ./SDRplay_RSP_API-Linux-ARM64-3.15.2.run

# 3. Enable the systemd service (the daemon must be running for the
#    modem to enumerate devices):
sudo systemctl enable --now sdrplay
systemctl status sdrplay     # must show "active (running)"

# 4. Plug the device and verify it shows up:
lsusb | grep -i sdrplay      # should list RSPduo / RSP1A / etc.
```

What the installer drops:

- `/usr/local/lib/libsdrplay_api.so.3.X` (+ symlinks) — the FFI library
- `/usr/local/include/sdrplay_api*.h` — headers that `bindgen` reads at build
- `/opt/sdrplay_api/sdrplay_apiService` — the daemon
- `/etc/udev/rules.d/66-sdrplay.rules` — USB permissions for non-root users
- `sdrplay.service` systemd unit

**Common pitfalls:**

| Symptom | Cause / fix |
|---|---|
| Device missing from the GUI dropdown | `systemctl status sdrplay` → not active. `sudo systemctl start sdrplay` |
| Daemon up but no device | `sudo udevadm control --reload && sudo udevadm trigger`, then unplug / replug |
| `Permission denied` on `/dev/bus/usb` | User not in `plugdev` (rare — the installer sets this up). `sudo usermod -aG plugdev $USER` then re-login |
| `modem-sdrplay` build fails (`fatal: sdrplay_api.h not found`) | Headers missing: reinstall the API. If in an atypical path, `export SDRPLAY_API_INCLUDE_DIR=/path/to/include SDRPLAY_API_LIB_DIR=/path/to/lib` before `cargo build` |
| GUI starts but device disappears when RX starts | API already opened by another process (SDRuno, SoapySDRPlay3 daemon). Close the other client |

The SDRplay API is needed **both at build time and runtime**: `build.rs`
runs `bindgen` against `/usr/local/include/sdrplay_api*.h` and panics
the compile with a clear message pointing at the install page if the
headers are missing. If you build the `.deb` on one machine and install
it on another, both need the API. To skip SDRplay support at build
time (ARM-only setup with no access to the headers, for instance),
disable the feature: `cargo tauri build --no-default-features
--features pluto`.

### ⚠ Sound card on Windows — make sure the default format is 48 kHz

**Symptom**: your sound card (typically a **SignaLink USB**, or any other
USB CM119-class interface) shows up in the *RX sound card* combo without
the *✓48k* tag, and *Start* stays greyed out. The GUI reports e.g.
`SignaLink USB — 44100–44100 Hz` instead of `48000–48000 Hz`.

**Cause**: on Windows, the shared-mode audio API (WASAPI) does not expose
the device's real capability list — only the **default format** configured
in the device's Windows properties. If Windows is set to 44.1 kHz, the
app sees 44.1 kHz, period. The SignaLink's CM119 chip natively supports
48 kHz but the shared layer hides it.

**Fix (one-time per sound card):**

1. Right-click the speaker icon (notification area) → *Sounds* (or:
   Control Panel → Hardware and Sound → Sound).
2. *Recording* tab → double-click the SignaLink input (usually
   `Microphone (USB Audio CODEC)`) → *Advanced* tab → *Default Format*
   list → pick **"2 channel, 16 bit, 48000 Hz (DVD Quality)"** → *Apply* /
   *OK*.
3. *Playback* tab → repeat the same on the SignaLink output (otherwise
   the TX chain will resample 48 → 44.1 → 48 kHz and degrade the modem
   signal).
4. Close and reopen the NBFM Modem; the input should now read
   `48000–48000 Hz ✓48k` and *Start* becomes clickable.

This applies to every USB-audio class interface (SignaLink, RIGblaster,
DigiRig, isolated CM108/CM119 cables, etc.). The setting is persistent —
only redo it if you switch cards or reinstall Windows.

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

> **For the SDRplay backend** (RSPduo / RSP1A / RSP1B / RSP1):
> install the SDRplay API 3.x **before** the build —
> [full procedure](#sdrplay-api-install-linux). Otherwise disable
> the feature: `cargo tauri build --no-default-features --features pluto`.

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

### Build — Raspberry Pi 5 (Debian 13 trixie, 7" touchscreen)

Native build identical to the Linux flow above, with two specifics:

- Debian 13's apt-rust ships Rust 1.85, **too old** for the `raptorq`
  2.0.1 crate (which requires ≥ 1.88 — `let-chains`,
  `unsigned_is_multiple_of`). Use `rustup`:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  source "$HOME/.cargo/env"
  ```

- For the official Raspberry Pi 7" touchscreen (DSI 800×480), a
  **dedicated kiosk build** auto-engages borderless fullscreen and a
  compact layout through `@media (max-width: 900px)`:

  ```bash
  cd rust/modem-gui/src-tauri
  cargo tauri build --bundles deb --config configs/tauri.pi.conf.json
  ```

  The bundled `.desktop` injects `Exec=env NBFM_KIOSK=1 …`; the Rust
  binary detects this env var at startup and applies `set_fullscreen`
  + `set_decorations(false)`. A ✕ button in the tab bar quits,
  **Escape** toggles fullscreen ↔ windowed. Standard build without
  `--config` keeps the desktop behavior. See
  [`rust/modem-gui/src-tauri/configs/README.md`](rust/modem-gui/src-tauri/configs/README.md).

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
