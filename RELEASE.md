# Release process

How to cut a new tag and publish binaries on GitHub Releases. Written
mechanically so an agent (or the next human) can follow it without
re-deriving the conventions.

## Versioning convention

Two version strings to keep in sync — they look almost identical but
the punctuation matters:

| Where | Format | Example |
|---|---|---|
| Git tag, GitHub release name, artifact filenames | **no hyphen** | `0.9.2rc5` |
| `rust/modem-gui/src-tauri/tauri.conf.json` (during build only) | **proper semver, with hyphen** | `0.9.2-rc5` |

Tauri's bundler rejects `0.9.2rc5` as not a semver string, so we
temporarily rewrite it with a hyphen before `cargo tauri build`, then
rename the produced artifacts to drop the hyphen and match the tag.
The bump is **not** committed — `tauri.conf.json` stays pinned at
`"version": "0.1.0"` in git (this has been the convention since the
first releases; the field is only consumed by tauri at build time).

Tag prefix: **no `v`**. The very first release used `v0.9.0-rc1`; from
`0.9.1rc2` onwards the prefix was dropped. Stick with no-`v`.

## Artifact matrix

A complete release ships seven files. Each row notes which build host
can produce it natively — Tauri bundling is platform-bound (webkit2gtk
on Linux, MSI on Windows), so cross-compilation is **not** an option.

| Artifact | Built on |
|---|---|
| `nbfm-modem-gui_<ver>_arm64.deb` | Pi (aarch64 Debian/RPiOS) |
| `nbfm-modem-gui_<ver>-pi5_arm64.deb` | Pi (aarch64 Debian/RPiOS) |
| `nbfm-modem-gui_<ver>_amd64.deb` | x86_64 Debian 12 |
| `nbfm-modem-gui_<ver>_amd64.AppImage` | x86_64 Debian 12 |
| `nbfm-modem-portable-linux-<ver>.tar.gz` | x86_64 Debian 12 (`make-portable.sh` is hard-coded `x86_64-unknown-linux-gnu`) |
| `nbfm-modem-gui_<ver>_x64-setup.exe` | Windows 10/11 x64 |
| `nbfm-modem-portable-<ver>.zip` | Windows 10/11 x64 |

Build prerequisites per host live in
[`rust/modem-gui/BUILD_DEBIAN.md`](rust/modem-gui/BUILD_DEBIAN.md) for
the Linux side. The Windows host needs the same Rust + tauri-cli plus
NSIS (auto-installed by `cargo tauri build` on first run).

## Per-host build steps

All commands assume the working tree is on the commit you want to ship
and `git status` is clean.

### Pi (arm64) — produces standard + Pi 5 kiosk `.deb`

```bash
cd ~/git/NewModem
VER=0.9.2rc5                     # tag form, no hyphen
TAURI_VER=0.9.2-rc5              # semver, with hyphen
mkdir -p dist/$VER

# 1) Bump tauri.conf.json (keep a backup, do NOT commit)
cp rust/modem-gui/src-tauri/tauri.conf.json{,.bak}
sed -i "s/\"version\": \"0.1.0\"/\"version\": \"$TAURI_VER\"/" \
    rust/modem-gui/src-tauri/tauri.conf.json

# 2) Standard arm64 .deb
cd rust/modem-gui/src-tauri
cargo tauri build --bundles deb
cp ../../target/release/bundle/deb/nbfm-modem-gui_${TAURI_VER}_arm64.deb \
   ../../../dist/$VER/nbfm-modem-gui_${VER}_arm64.deb

# 3) Pi 5 kiosk variant (overwrites the deb in target/, so do this last
#    or copy the standard one out first — step 2 already did)
cargo tauri build --bundles deb --config configs/tauri.pi.conf.json
cp ../../target/release/bundle/deb/nbfm-modem-gui_${TAURI_VER}_arm64.deb \
   ../../../dist/$VER/nbfm-modem-gui_${VER}-pi5_arm64.deb

# 4) Restore tauri.conf.json
mv tauri.conf.json.bak tauri.conf.json
cd ../../..

ls -lh dist/$VER/
```

The two debs are byte-identical except for `usr/share/applications/*.desktop`:

- **standard**: `Exec=nbfm-modem-gui`, name `nbfm-modem-gui`
- **pi5**: `Exec=env NBFM_KIOSK=1 nbfm-modem-gui`, name `NBFM Modem RX`

`NBFM_KIOSK=1` is read by `modem-gui` at startup and engages
borderless fullscreen for the official 800×480 DSI touchscreen.

### Debian x86_64 — produces amd64 `.deb` + AppImage + portable Linux

```bash
cd ~/git/NewModem
VER=0.9.2rc5
TAURI_VER=0.9.2-rc5
mkdir -p dist/$VER

cp rust/modem-gui/src-tauri/tauri.conf.json{,.bak}
sed -i "s/\"version\": \"0.1.0\"/\"version\": \"$TAURI_VER\"/" \
    rust/modem-gui/src-tauri/tauri.conf.json

cd rust/modem-gui/src-tauri
cargo tauri build --bundles deb,appimage
cp ../../target/release/bundle/deb/nbfm-modem-gui_${TAURI_VER}_amd64.deb \
   ../../../dist/$VER/nbfm-modem-gui_${VER}_amd64.deb
cp ../../target/release/bundle/appimage/nbfm-modem-gui_${TAURI_VER}_amd64.AppImage \
   ../../../dist/$VER/nbfm-modem-gui_${VER}_amd64.AppImage

mv tauri.conf.json.bak tauri.conf.json
cd ../../..

# Portable Linux tar.gz — version comes from `git describe`, so tag first
# (or pass it explicitly).
./make-portable.sh $VER
cp dist/portable/nbfm-modem-portable-linux-$VER.tar.gz dist/$VER/
```

### Windows x64 — produces NSIS `.exe` + portable `.zip`

```powershell
cd C:\path\to\NewModem
$VER = "0.9.2rc5"
$TAURI_VER = "0.9.2-rc5"
mkdir dist\$VER -Force

# Bump tauri.conf.json (no commit)
Copy-Item rust\modem-gui\src-tauri\tauri.conf.json `
          rust\modem-gui\src-tauri\tauri.conf.json.bak
(Get-Content rust\modem-gui\src-tauri\tauri.conf.json) `
    -replace '"version": "0.1.0"', "`"version`": `"$TAURI_VER`"" |
    Set-Content rust\modem-gui\src-tauri\tauri.conf.json

cd rust\modem-gui\src-tauri
cargo tauri build --bundles nsis
Copy-Item ..\..\target\release\bundle\nsis\nbfm-modem-gui_${TAURI_VER}_x64-setup.exe `
          ..\..\..\dist\$VER\nbfm-modem-gui_${VER}_x64-setup.exe

Move-Item -Force tauri.conf.json.bak tauri.conf.json
cd ..\..\..

# Portable zip — see rust/modem-gui/portable/ for the staging script /
# manual assembly steps used previously.
```

## Tag, release, upload

Once **at least one host's** artifacts are ready, create the GitHub
release as a draft and attach what you have. Other hosts can upload to
the same draft later.

```bash
# 1) Tag the commit (annotated, with release notes)
git tag -a $VER -m "$VER — <one-line summary>

<longer description, in English, explaining what changed and why
users should upgrade>" <commit-sha>

git push origin main      # if any new commits are local
git push origin $VER

# 2) Create a draft release attached to the tag
gh release create $VER \
  --title "$VER — <short title>" \
  --notes-file release-notes.md \
  --draft \
  dist/$VER/<artifact1> \
  dist/$VER/<artifact2>

# 3) Upload more assets later from another host
gh release upload $VER dist/$VER/<more-artifact>

# 4) Publish — flips draft=false and (optionally) marks as latest
gh release edit $VER --draft=false --latest
```

Release notes live on GitHub only — there is no `CHANGELOG.md` in the
repo. Keep them in English (per CLAUDE.md), one section per notable
change, plus a short "Artifacts" section listing what's attached.

## Hotfix release (single-commit, single-platform first)

When a fix lands and you want to publish quickly even before all
binaries are ready (e.g. the disk-fill fix in `0.9.2rc5`):

1. Land the fix on `main`, push.
2. Build whatever artifacts the host you're on can produce (typically
   the Pi → arm64 + pi5 deb).
3. Tag, push tag, create the **draft** release with those artifacts
   only.
4. Tell users to wait for the publish (the draft is invisible to
   non-collaborators).
5. Build the remaining artifacts on the other hosts, `gh release
   upload` each.
6. Once everything is attached, `gh release edit … --draft=false
   --latest` to publish.

Drafts have an `untagged-<hash>` URL slug until publication; this is
normal and the slug becomes `tag/<ver>` once published.

## Sanity checks before publishing

A 30-second smoke test that catches most regressions:

```bash
# Verify the fix is actually in the binary (replace with the relevant
# string for the release in question)
strings dist/$VER/nbfm-modem-gui_${VER}_arm64.deb |
    strings | grep -E "<expected new string>"

# Verify the two arm64 debs differ only as expected
mkdir -p /tmp/cmp && cd /tmp/cmp
dpkg-deb -x ~/git/NewModem/dist/$VER/nbfm-modem-gui_${VER}_arm64.deb std/
dpkg-deb -x ~/git/NewModem/dist/$VER/nbfm-modem-gui_${VER}-pi5_arm64.deb pi5/
diff -r std/ pi5/        # should differ ONLY in usr/share/applications/*.desktop
sha256sum std/usr/bin/* pi5/usr/bin/*   # binaries must match exactly
```

For a non-hotfix release also smoke-test the GUI manually: `sudo apt
install ./nbfm-modem-gui_${VER}_arm64.deb`, launch, confirm the
sound-card dropdown populates, run a short RX session.
