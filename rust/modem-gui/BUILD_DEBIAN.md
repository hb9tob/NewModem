# Build .deb on Debian 12 — agent notes

Target: produce `nbfm-modem-gui_0.1.0_amd64.deb` from a fresh Debian 12
(bookworm) machine. Intended to be followed mechanically by an agent.

## 0. Sanity check

```bash
lsb_release -a
# Expect: Description: Debian GNU/Linux 12 (bookworm)

apt-cache show libwebkit2gtk-4.1-dev | head -3
# Must return a package. If not: wrong distro, abort.

uname -m
# Must be x86_64. ARM needs a different target triple.
```

## 1. System dependencies

```bash
sudo apt update
sudo apt install -y \
  build-essential curl wget file pkg-config \
  libwebkit2gtk-4.1-dev \
  libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev \
  libasound2-dev \
  git
```

`libasound2-dev` is required for `cpal` (ALSA backend).
`libwebkit2gtk-4.1-dev` is required for Tauri 2 (NOT `-4.0`).

## 2. Rust toolchain

Debian 12's apt ships Rust 1.63 — too old for Tauri 2 (needs 1.77+).
Use rustup:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version   # expect >= 1.77
```

## 3. Tauri CLI v2

```bash
cargo install tauri-cli --version "^2" --locked
# Takes ~3-5 min the first time.
cargo tauri --version   # expect: tauri-cli 2.x.y
```

## 4. Clone and build

```bash
git clone <REPO_URL> NewModem
cd NewModem/rust/modem-gui/src-tauri

# Full bundle (deb + appimage).
cargo tauri build

# OR: .deb only (faster).
cargo tauri build --bundles deb
```

First build downloads and compiles all Tauri + cpal deps: **10–20 min**
on a modern machine, longer on slow disk.

## 5. Locate artifacts

```bash
ls -lh ../../target/release/bundle/deb/
# nbfm-modem-gui_0.1.0_amd64.deb
```

The binary alone (without packaging) is at
`rust/target/release/nbfm-modem-gui`.

## 6. Install and validate

```bash
DEB=$(ls ../../target/release/bundle/deb/nbfm-modem-gui_*.deb | head -1)
sudo apt install -y "$DEB"
# apt auto-resolves runtime deps (webkit2gtk-4.1-0, gtk3, alsa).

# Launch (GUI session required).
nbfm-modem-gui &
```

Validation checklist:
- [ ] Window "NBFM Modem RX" opens
- [ ] Top-bar dropdown lists input sound cards with sample rate
- [ ] Status text shows `N entrée(s)` (N > 0 if a mic / line-in exists)
- [ ] No Tauri/webkit error in stderr

Uninstall:

```bash
sudo apt remove -y nbfm-modem-gui
```

## 7. CLI TX (for OTA testing)

The `modem-cli` crate builds the `nbfm-modem` binary (TX / RX via WAV
files). It has no extra system deps beyond `build-essential` — Rust +
pure-Rust crates (hound, clap). If steps 1–2 were done, the build is
immediate.

```bash
cd NewModem/rust
cargo build -p modem-cli --release
./target/release/nbfm-modem --help
```

**OTA TX example** — encode an image into a WAV, then play the WAV
through the radio's audio input (PTT triggered by VOX preamble) :

```bash
./target/release/nbfm-modem tx \
  --input image.avif \
  --output tx.wav \
  --profile NORMAL \
  --frame-version 2 \
  --filename image.avif \
  --callsign HB9TOB \
  --session-id DEADBEEF \
  --vox 0.8

# Play tx.wav on the sound card connected to the radio's mic input :
aplay -D plughw:CARD=UMC,DEV=0 tx.wav   # adjust CARD= to your device
# or via PulseAudio / PipeWire :
paplay tx.wav
```

Profile quick-guide :
- `ULTRA` / `ROBUST` : lowest baud, best resilience — use for weak links
- `NORMAL` : balanced default, ~1500 Bd
- `HIGH` / `MEGA` : fastest, clean channels only

The receiver (CLI `rx` or the GUI) must use the same `--frame-version 2`
and any LDPC/rate overrides you set on TX. The protocol header embeds
the profile index, so the GUI auto-detects it on reception.

## Troubleshooting

| Symptom | Cause / Fix |
|---|---|
| `Package libwebkit2gtk-4.1-dev has no installation candidate` | Not Debian 12. Check `/etc/os-release`. |
| `cargo: command not found` after rustup | Forgot `source "$HOME/.cargo/env"`, or new shell needs `~/.bashrc` reload. |
| `failed to run custom build command for tauri` | Missing system deps → re-run step 1. Specifically `pkg-config` + `libwebkit2gtk-4.1-dev`. |
| `error: linker 'cc' not found` | Missing `build-essential`. |
| `.deb` install fails `depends: libwebkit2gtk-4.1-0` | Runtime lib missing: `sudo apt install libwebkit2gtk-4.1-0`. |
| App launches but dropdown stuck on "Chargement…" | Open devtools (right-click → Inspect). Check console for Tauri API errors. |
| No sound cards listed | Expected in headless containers. Run on a real desktop session or pass `--device /dev/snd` if in Docker. |

## Cross-compile (NOT recommended)

Do not cross-compile Tauri from another OS. Webview2gtk and its native
deps make cross builds extremely fragile. Always build on a native
Debian/Ubuntu matching the deployment target.

## File manifest produced

After a successful build the following are created under
`NewModem/rust/target/`:

```
release/nbfm-modem-gui              # stripped ELF binary
release/bundle/deb/*.deb            # Debian package
release/bundle/appimage/*.AppImage  # (if built)
```

The `gen/schemas/` directory under `src-tauri/` is also regenerated —
it is gitignored and should not be committed.
