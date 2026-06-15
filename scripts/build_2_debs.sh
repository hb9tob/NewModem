#!/usr/bin/env bash
# Build the 2 .deb variants on the Pi dev box (post-libloading convention).
# - base   : default features, normal desktop entry
# - pi4+5  : default features, kiosk desktop entry (configs/tauri.pi.conf.json)
# SDRplay/RTL-SDR are libloading-based, so they ship in the default set; no
# more -sdrplay suffix.
set -eu -o pipefail

REPO="$HOME/git/NewModem"
SRC_TAURI="$REPO/rust/modem-gui/src-tauri"
TAURI_CONF="$SRC_TAURI/tauri.conf.json"
BUILD_OUT="$REPO/rust/target/release/bundle/deb"
OUT_DIR="$HOME/nbfm-debs"

# Version is whatever tauri.conf.json already declares.
BASE_VER="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["version"])' "$TAURI_CONF")"

mkdir -p "$OUT_DIR"
export PATH="$HOME/.cargo/bin:$PATH"

stage() {
  local name="$1" suffix="$2"; shift 2
  echo "=== BUILD :: ${name} (${BASE_VER}${suffix}) :: args=$* ==="
  # Force re-bundle (Tauri generate_context! caching trap).
  touch "$SRC_TAURI/src/main.rs"
  ( cd "$SRC_TAURI" && cargo tauri build --bundles deb "$@" )
  local src
  src=$(ls -t "$BUILD_OUT"/nbfm-modem-gui_*_arm64.deb 2>/dev/null | head -1)
  [[ -f "$src" ]] || { echo "!!! MISSING OUTPUT in $BUILD_OUT" >&2; exit 1; }
  local dst="$OUT_DIR/nbfm-modem-gui_${BASE_VER}${suffix}_arm64.deb"
  cp "$src" "$dst"
  echo "OK :: $dst ($(du -h "$dst" | cut -f1))"
}

t0=$(date +%s)
stage "base"  ""
stage "kiosk" "-pi4+5"  --config configs/tauri.pi.conf.json
t1=$(date +%s)

echo "=== BOTH DEBS DONE in $((t1 - t0))s ==="
ls -lh "$OUT_DIR"/nbfm-modem-gui_${BASE_VER}*.deb
