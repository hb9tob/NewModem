# syntax=docker/dockerfile:1.7

FROM debian:bookworm-slim AS builder

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:${PATH} \
    APPIMAGE_EXTRACT_AND_RUN=1

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        file \
        libasound2-dev \
        libayatana-appindicator3-dev \
        librsvg2-dev \
        libssl-dev \
        libudev-dev \
        libwebkit2gtk-4.1-dev \
        libxdo-dev \
        patchelf \
        pkg-config \
        squashfs-tools \
        wget \
        xdg-utils \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable \
    && cargo install tauri-cli --version '^2' --locked

WORKDIR /workspace
COPY rust/ rust/
COPY --chmod=0755 scripts/fix_appimage_egl.sh /usr/local/bin/fix-appimage-egl

# SDRplay's SDK cannot be redistributed. This portable build keeps the Pluto
# and runtime-loaded RTL-SDR backends and omits SDRplay support.
RUN --mount=type=cache,target=/opt/cargo/registry \
    --mount=type=cache,target=/opt/cargo/git \
    --mount=type=cache,target=/workspace/rust/target \
    cd rust/modem-gui/src-tauri \
    && cargo tauri build --bundles appimage -- \
        --no-default-features --features pluto,rtlsdr \
    && appimage="$(find ../../target/release/bundle/appimage -maxdepth 1 \
        -type f -name '*.AppImage' -print -quit)" \
    && test -n "$appimage" \
    && fix-appimage-egl "$appimage" /out/nbfm-modem-gui-x86_64.AppImage

FROM scratch AS artifact
COPY --from=builder /out/ /