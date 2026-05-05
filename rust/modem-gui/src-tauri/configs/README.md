# Tauri config overlays

Overlays merged on top of `../tauri.conf.json` at build time via
`cargo tauri build --config <path>`.

## `tauri.pi.conf.json` — Raspberry Pi 7" touchscreen variant

For the 800×480 official Pi touchscreen (DSI). Layers in:

- `bundle.linux.deb.desktopTemplate = "templates/main.desktop"`
  → the system menu launcher gets `Exec=env NBFM_KIOSK=1 ...` and
    `Name=NBFM Modem RX`. The `NBFM_KIOSK=1` env triggers the Rust
    setup hook to engage fullscreen + `decorations:false` so the app
    covers the labwc panel like a kiosk.

The responsive layout (`@media (max-width: 900px)` in `style.css`)
applies regardless of build target — it kicks in by viewport width.
This overlay only changes the *launcher*, not the binary.

### Build

```bash
cd rust/modem-gui/src-tauri
cargo tauri build --bundles deb --config configs/tauri.pi.conf.json
```

The resulting `nbfm-modem-gui_<ver>_arm64.deb` ships a kiosk-enabled
`/usr/share/applications/nbfm-modem-gui.desktop`.

### Standard Linux/Windows builds

Don't pass `--config` — the base `tauri.conf.json` produces a regular
desktop `.deb` (and `.exe` / AppImage) without the kiosk env.
