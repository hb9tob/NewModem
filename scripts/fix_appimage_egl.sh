#!/usr/bin/env bash

# Tauri's linuxdeploy version bundles an old display stack but leaves EGL and
# Mesa to the host. On rolling distributions that mixture makes WebKitGTK abort
# with EGL_BAD_PARAMETER before the window appears. Remove only the libraries
# confirmed by tauri-apps/tauri#15976, then rebuild with the original runtime.

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "Usage: $0 INPUT.AppImage OUTPUT.AppImage" >&2
    exit 2
fi

source_appimage="$(readlink -f "$1")"
output_dir="$(dirname "$2")"
mkdir -p "$output_dir"
output_appimage="$(cd "$output_dir" && pwd)/$(basename "$2")"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

cd "$work_dir"
APPIMAGE_EXTRACT_AND_RUN=1 "$source_appimage" --appimage-extract >/dev/null

library_patterns=(
    'libwayland-client.so*'
    'libwayland-cursor.so*'
    'libwayland-egl.so*'
    'libwayland-server.so*'
    'libxkbcommon.so*'
    'libxcb-randr.so*'
    'libxcb-render.so*'
    'libxcb-shm.so*'
    'libXau.so*'
    'libXdmcp.so*'
)

shopt -s nullglob
removed=0
for pattern in "${library_patterns[@]}"; do
    matches=(squashfs-root/usr/lib/$pattern)
    if (( ${#matches[@]} > 0 )); then
        removed=$((removed + ${#matches[@]}))
        rm -f -- "${matches[@]}"
    fi
done

if (( removed == 0 )); then
    echo "No bundled display-stack libraries found; refusing an unverified repack" >&2
    exit 1
fi

runtime_size="$($source_appimage --appimage-offset)"
if [[ ! "$runtime_size" =~ ^[0-9]+$ ]] || (( runtime_size == 0 )); then
    echo "Could not determine the AppImage runtime size" >&2
    exit 1
fi

head -c "$runtime_size" "$source_appimage" > runtime
mksquashfs squashfs-root filesystem.squashfs \
    -noappend -all-root -comp zstd >/dev/null

temporary_output="$output_appimage.tmp"
cat runtime filesystem.squashfs > "$temporary_output"
chmod 0755 "$temporary_output"
mv -f "$temporary_output" "$output_appimage"

echo "Removed $removed incompatible display-stack libraries"