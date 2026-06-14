#!/usr/bin/env bash
# Build 4 .deb variants on Pi5 dev box.
# - base (no SDR, no kiosk)
# - sdrplay (with SDR, no kiosk)
# - pi4+5 (no SDR, kiosk)
# - pi4+5-sdrplay (with SDR, kiosk)
set -eu -o pipefail

BASE_VER="0.13.5"
OUT_DIR="$HOME/nbfm-debs"
REPO="$HOME/git/NewModem"
SRC_TAURI="$REPO/rust/modem-gui/src-tauri"
TAURI_CONF="$SRC_TAURI/tauri.conf.json"
BUILD_OUT="$REPO/rust/target/release/bundle/deb"

mkdir -p "$OUT_DIR"

# Backup tauri.conf.json so we can restore after the run
cp "$TAURI_CONF" "$TAURI_CONF.run.bak"
trap 'mv "$TAURI_CONF.run.bak" "$TAURI_CONF" 2>/dev/null || true' EXIT

set_version() {
  local v="$1"
  python3 - "$TAURI_CONF" "$v" <<'PY'
import json, sys
p, v = sys.argv[1], sys.argv[2]
with open(p) as f: c = json.load(f)
c['version'] = v
with open(p, 'w') as f:
    json.dump(c, f, indent=2)
    f.write('\n')
PY
  echo "[set_version] $v"
}

stage() {
  local n="$1" ver="$2"; shift 2
  echo "=== BUILD #${n} :: version=${ver} :: args=$* ==="
  set_version "$ver"
  # Force re-bundle by touching main.rs (Tauri generate_context! caching trap)
  touch "$SRC_TAURI/src/main.rs"
  ( cd "$SRC_TAURI" && cargo tauri build --bundles deb "$@" )
  # Find the freshest .deb (Tauri may sanitize special chars like '+' in the filename)
  local src
  src=$(ls -t "$BUILD_OUT"/nbfm-modem-gui_*_arm64.deb 2>/dev/null | head -1)
  if [[ -z "$src" || ! -f "$src" ]]; then
    echo "!!! MISSING OUTPUT in $BUILD_OUT" >&2
    ls -la "$BUILD_OUT" >&2
    exit 1
  fi
  # Always name the destination with our requested version
  local dst="$OUT_DIR/nbfm-modem-gui_${ver}_arm64.deb"
  cp "$src" "$dst"
  echo "BUILD #${n} OK :: $dst ($(du -h "$dst" | cut -f1))"
}

t0=$(date +%s)

stage 1 "${BASE_VER}"                                                                -- --no-default-features --features pluto
stage 2 "${BASE_VER}-sdrplay"
stage 3 "${BASE_VER}-pi4+5"           --config configs/tauri.pi.conf.json -- --no-default-features --features pluto
stage 4 "${BASE_VER}-pi4+5-sdrplay"   --config configs/tauri.pi.conf.json

t1=$(date +%s)
echo "ALL 4 BUILDS DONE in $((t1 - t0))s"
ls -lh "$OUT_DIR"/nbfm-modem-gui_${BASE_VER}*.deb
