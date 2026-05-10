#!/usr/bin/env bash
# Build un tar.gz portable Linux du modem NBFM.
#
# Construit CLI + GUI en release puis assemble dans dist/portable/ un dossier
# pret a tar.gz contenant :
#   - nbfm-modem-gui                                  (binaire GUI Tauri)
#   - nbfm-modem-x86_64-unknown-linux-gnu             (sidecar CLI renomme)
#   - portable.txt                                    (marqueur mode portable)
#   - README-portable.txt                             (notice utilisateur)
#
# Le marqueur portable.txt declenche le helper Rust portable_root() qui
# redirige settings, captures RX et sessions dans <exe_dir>/data/.
#
# Le secret HMAC du collecteur est embarque dans le binaire au build
# (include_str! sur secret.txt). L'archive publiee partage donc ce secret
# avec le collector serveur.
#
# Usage :
#   ./make-portable.sh                  # tag = git describe
#   ./make-portable.sh v0.1.0-test      # tag explicite
#   ./make-portable.sh "" --skip-build  # saute les cargo build
#
# Prerequis Debian/Ubuntu :
#   sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
#                    libayatana-appindicator3-dev librsvg2-dev \
#                    libssl-dev pkg-config build-essential

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$REPO_ROOT/rust"
TARGET_DIR="$RUST_DIR/target/release"
DIST_ROOT="$REPO_ROOT/dist/portable"

if [ ! -d "$RUST_DIR" ]; then
    echo "rust/ introuvable sous $REPO_ROOT" >&2
    exit 1
fi

TAG="${1:-}"
SKIP_BUILD=0
if [ "${2:-}" = "--skip-build" ]; then
    SKIP_BUILD=1
fi
if [ -z "$TAG" ]; then
    TAG="$(cd "$REPO_ROOT" && git describe --tags --always 2>/dev/null || echo dev)"
fi

TRIPLE="x86_64-unknown-linux-gnu"
STAGE_NAME="nbfm-modem-portable-linux-$TAG"
STAGE_DIR="$DIST_ROOT/$STAGE_NAME"
ARCHIVE_PATH="$DIST_ROOT/$STAGE_NAME.tar.gz"

echo "[portable] tag       = $TAG"
echo "[portable] staging   = $STAGE_DIR"
echo "[portable] archive   = $ARCHIVE_PATH"

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "[portable] cargo build --release -p modem-cli"
    (cd "$RUST_DIR" && cargo build --release -p modem-cli)
    echo "[portable] cargo build --release -p modem-gui"
    (cd "$RUST_DIR" && cargo build --release -p modem-gui)
else
    echo "[portable] --skip-build : saute les cargo build"
fi

GUI_BIN="$TARGET_DIR/nbfm-modem-gui"
CLI_BIN="$TARGET_DIR/nbfm-modem"
for p in "$GUI_BIN" "$CLI_BIN"; do
    if [ ! -f "$p" ]; then
        echo "Binaire absent : $p" >&2
        exit 2
    fi
done

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

cp "$GUI_BIN" "$STAGE_DIR/nbfm-modem-gui"
cp "$CLI_BIN" "$STAGE_DIR/nbfm-modem-$TRIPLE"
chmod +x "$STAGE_DIR/nbfm-modem-gui" "$STAGE_DIR/nbfm-modem-$TRIPLE"

# Marqueur portable : presence suffit, contenu ignore par le code Rust.
: > "$STAGE_DIR/portable.txt"

README_SRC="$REPO_ROOT/rust/modem-gui/portable/README-portable.txt"
if [ -f "$README_SRC" ]; then
    cp "$README_SRC" "$STAGE_DIR/README-portable.txt"
else
    echo "Warning: README-portable.txt introuvable a $README_SRC" >&2
fi

rm -f "$ARCHIVE_PATH"
(cd "$DIST_ROOT" && tar -czf "$STAGE_NAME.tar.gz" "$STAGE_NAME")

SIZE_MB=$(du -m "$ARCHIVE_PATH" | cut -f1)
echo
echo "[portable] OK"
echo "[portable] $ARCHIVE_PATH  (${SIZE_MB} MB)"
