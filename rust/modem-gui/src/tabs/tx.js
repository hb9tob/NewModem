// TX tab — source loading (picker + native drag-drop), AVIF/zstd
// compression with live preview, transport-limit gating, the fountain
// estimate, the TX/More/Stop orchestration (half- and full-duplex), the
// duplex progress bar, and History relay/resume. Depends on rx.js for the
// shared #progress-blocks bitmap (half-duplex repaints it via
// setProgressBitmap); the tx->rx edge is acyclic (rx.js imports nothing
// from here). The tx:* bus subscriptions stay wired centrally in main.js
// setupSdrBusHandlers, mirroring the rx.js extraction.
import { invoke, listen, convertFileSrc } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings, modemProfiles } from "../lib/state.js";
import { logEvent } from "../lib/log.js";
import { fmtSeconds, txFormatBytes, isImageFilename } from "../lib/format.js";
import { rxIsRunning } from "../lib/dom.js";
import { startCapture } from "../lib/capture.js";
import { getActiveOverlayPayload } from "./overlays.js";
import { persistSettings } from "./settings.js";
import { setProgressBitmap, drawProgressBlocks, hideFountainStatus } from "./rx.js";

export const txState = {
  sourceFile: null,
  sourceImage: null,
  sourceSize: 0,
  sourceUrl: null,
  mode: "HIGH",
  resize: "800x600",
  freeW: 640,
  freeH: 480,
  // Default 10: size/quality trade-off usable out-of-the-box on a NBFM pass.
  // Persisted across sessions (cf. applyTxSettingsToUI).
  quality: 10,
  // AVIF encoder speed, 1..=10. 6 = balanced (a few seconds on an SP7),
  // 1 = max compression/very slow, 10 = fast but larger file.
  speed: 6,
  // % of RaptorQ repair blocks added to the initial burst (0, 5, 10, 20,
  // 30, 50, 100). 5 = modest default (user bumps it as needed via
  // "TX more"). Persisted across sessions.
  repairPct: 5,
  // True when the source is already an AVIF: we emit the bytes as-is,
  // with no decoding or re-encoding (no loss, no CPU cycles).
  avifPassthrough: false,
  // True when the source is not an image - we switch to compress_file_zstd
  // (lossless) instead of compress_image. No image preview, no resizing,
  // 10 min limit instead of 5.
  fileMode: false,
  // Number of blocks to emit as a "More" burst (exact value, not a %).
  // User picks from a discrete select or enters a free value.
  // Typical use case: "I'm missing 5 blocks" -> count = 5.
  moreCount: 5,
  aspectLinked: true,
  txActive: false,
  // Additional fountain blocks to generate on TX more (% of code size).
  morePct: 20,
  // State of the in-progress TX session: kept between the initial TX and
  // successive "More" bursts so we can continue ESI without overlapping
  // packets already emitted. Reset when image or mode change.
  lastTx: null,  // { esiMax, mode }
  // Path of the tx_history archive backing the CURRENT session, learnt from
  // tx_start's return value (fresh TX) or tx_resume (resumed session). Used
  // to persist the ESI high-water (tx_set_next_esi) after every burst so the
  // fountain can be continued later. Null = not archived yet.
  archivePath: null,
  // When a session is resumed from history, the callsign that was used at
  // the original TX. It MUST be reused for the continuation bursts, else the
  // session_id (which depends on the callsign) wouldn't match and the RX
  // would treat the extra blocks as a brand-new session. Null = use the
  // current settings callsign (fresh sessions).
  resumeCallsign: null,
  compressedBytes: null,
  compressedUrl: null,
  compressing: false,
  compressTimer: null,
  compressSeq: 0,
  // True when a parameter (quality / resize / free dimensions) has been
  // modified since the last successful compression. Drives the "stale"
  // indicator + the warn style of the Recompute button.
  compressDirty: false,
  // Anti-reentrance guard: drop ignored while an image is loading
  // (avoids two parallel loadTxFileFromPath calls).
  loading: false,
  // Estimate computed by the backend after each compression or mode
  // change; drives TX button activation and the "estimated duration ·
  // block count" display.
  estimate: null,
  // Tracking of an in-progress transmission.
  progress: null,
  restartRxAfter: false,
};

export function clearTxSessionRef() {
  txState.lastTx = null;
  txState.archivePath = null;
  txState.resumeCallsign = null;
}

export let _compressChain = Promise.resolve();

export const TX_HARD_BYTES = 100 * 1024;

export const TX_HARD_SECONDS = 5 * 60;

export const TX_WARN_SECONDS = 2 * 60;

export const TX_FILE_HARD_SECONDS = 10 * 60;

export const TX_FILE_WARN_SECONDS = 5 * 60;

export function refreshTxExperimentalWarn() {
  const warn = document.getElementById("tx-experimental-warn");
  if (!warn) return;
  // Source of truth: ProfileDescriptor.experimental from modem-core (cf.
  // V3Modem in modem-core/src/v3_modem.rs). Adding/removing an experimental
  // profile in core automatically re-flags the warning here.
  const desc = modemProfiles.find((p) => p.name === txState.mode);
  warn.hidden = !(desc && desc.experimental);
}

export function refreshTxButtons() {
  const btnTx = document.getElementById("tx-btn-tx");
  const btnStop = document.getElementById("tx-btn-stop");
  const btnMore = document.getElementById("tx-btn-more");
  const btnCompress = document.getElementById("tx-btn-compress");
  const repairPct = document.getElementById("tx-repair-pct");
  const moreCount = document.getElementById("tx-more-count");
  if (!btnTx) return;
  const hasSource = !!txState.sourceFile;
  const isFile = !!txState.fileMode;
  const hasCompressed = txState.compressedBytes != null;
  const est = txState.estimate;
  if (btnCompress) {
    btnCompress.disabled =
      !hasSource || txState.compressing || txState.txActive;
    if (txState.compressing) {
      btnCompress.textContent = isFile ? "Compression zstd…" : "Compression…";
    } else if (txState.compressDirty) {
      btnCompress.textContent = "⚠ Recalculer la compression";
    } else {
      btnCompress.textContent = "Recalculer la compression";
    }
    btnCompress.classList.toggle(
      "tx-btn-warn",
      txState.compressDirty && !txState.compressing,
    );
  }

  // Limites : image = 100 ko + 5 min ; fichier = 10 min (taille libre).
  const bytes = txState.compressedBytes || 0;
  const dur = est ? est.duration_s : 0;
  const hardSeconds = isFile ? TX_FILE_HARD_SECONDS : TX_HARD_SECONDS;
  const warnSeconds = isFile ? TX_FILE_WARN_SECONDS : TX_WARN_SECONDS;
  const tooBig = !isFile && bytes > TX_HARD_BYTES;
  const tooLong = dur > hardSeconds;
  const warn = dur > warnSeconds && !tooLong;

  const canTx = hasSource
    && hasCompressed
    && !txState.compressing
    && !txState.txActive
    && !tooBig
    && !tooLong;
  btnTx.disabled = !canTx;
  const hasPriorTx =
    txState.lastTx != null && txState.lastTx.mode === txState.mode;
  btnMore.disabled = !hasSource || txState.txActive || !hasPriorTx;
  btnMore.title = moreButtonTitle();
  btnStop.disabled = !txState.txActive;
  if (repairPct) repairPct.disabled = !hasSource || txState.txActive;
  if (moreCount) moreCount.disabled = !hasSource || txState.txActive;

  // TX button label + color depending on state.
  if (txState.txActive) {
    btnTx.textContent = t("tx.btn_running");
    btnTx.title = t("status.tx_in_progress");
  } else if (tooBig) {
    btnTx.textContent = t("tx.btn_too_big");
    btnTx.title = t("status.tx_oversize_kio", { size: (bytes / 1024).toFixed(1) });
  } else if (tooLong) {
    const limMin = isFile ? 10 : 5;
    btnTx.textContent = t("tx.btn_too_long", { limit: limMin });
    btnTx.title = t("status.tx_oversize_time", { dur: fmtSeconds(dur), limit: limMin });
  } else if (warn) {
    btnTx.textContent = `TX ⚠ ${fmtSeconds(dur)}`;
    btnTx.title = txButtonTitle(est, dur, true);
  } else if (est) {
    btnTx.textContent = `TX (${fmtSeconds(dur)})`;
    btnTx.title = txButtonTitle(est, dur, false);
  } else {
    btnTx.textContent = "TX";
    btnTx.title = "";
  }
  btnTx.classList.toggle("tx-btn-warn", warn && !txState.txActive);
  // Kiosk: WebKitGTK on the Pi 7" touchscreen turns the native
  // title-based tooltip into a sticky bubble that doesn't auto-hide
  // on tap-release. We suppress it here and show a JS-controlled
  // toast (showKioskInfoToast) on TX press instead. Desktop hover
  // tooltips are unaffected.
  if (document.body.classList.contains("kiosk-mode")) {
    btnTx.removeAttribute("title");
  }
}

export function txButtonTitle(est, dur, longTx) {
  if (!est) return "";
  const base = longTx ? t("tx.dur_long") : t("tx.dur");
  const k = est.k_source;
  const n = est.n_initial ?? est.total_blocks;
  const parts = [t("tx.dur_blocks", { base, dur: fmtSeconds(dur), n })];
  if (k != null && k !== n) {
    parts.push(t("tx.parts_with_k", { k }));
  }
  if (est.duration_s_k != null) {
    parts.push(t("tx.threshold_lossless", { dur: fmtSeconds(est.duration_s_k) }));
  }
  return parts.join(" · ");
}

export function moreButtonTitle() {
  const est = txState.estimate;
  const count = computeMoreCount();
  if (!est || !est.seconds_per_cw) {
    return t("tx.emit_n_more", { count });
  }
  const dur = est.seconds_per_cw * count;
  return t("tx.emit_n_more_dur", { count, dur: fmtSeconds(dur) });
}

export function txFitInto(w, h, maxW, maxH) {
  const s = Math.min(maxW / w, maxH / h, 1);
  return { w: Math.max(1, Math.round(w * s)), h: Math.max(1, Math.round(h * s)) };
}

export function txTargetDims() {
  const src = txState.sourceImage;
  if (!src) return null;
  const w = src.naturalWidth;
  const h = src.naturalHeight;
  switch (txState.resize) {
    case "none":
      return { w, h };
    case "1920x1024":
      return txFitInto(w, h, 1920, 1024);
    case "800x600":
      return txFitInto(w, h, 800, 600);
    case "free":
      return { w: txState.freeW, h: txState.freeH };
    default:
      return { w, h };
  }
}

export function refreshTxPreview() {
  const info = document.getElementById("tx-preview-info");
  const srcSize = document.getElementById("tx-source-size");
  const cmpSize = document.getElementById("tx-compressed-size");
  const hasSource = !!txState.sourceFile;
  if (!hasSource) {
    if (info) info.textContent = "—";
    if (srcSize) srcSize.textContent = "—";
    if (cmpSize) cmpSize.textContent = "—";
    return;
  }
  if (txState.fileMode) {
    // No dimensions to display for a non-image file - show the original
    // filename and the current modem mode.
    if (info) {
      const cmpPart = txState.compressing ? " · zstd…" : "";
      info.textContent = `${txState.sourceFile.name} · zstd · ${txState.mode}${cmpPart}`;
    }
  } else if (txState.sourceImage) {
    const natW = txState.sourceImage.naturalWidth;
    const natH = txState.sourceImage.naturalHeight;
    const d = txTargetDims();
    if (info) {
      const resizePart = d.w === natW && d.h === natH
        ? `${natW}×${natH}`
        : `${natW}×${natH} → ${d.w}×${d.h}`;
      const cmpPart = txState.compressing ? " · compression…" : "";
      info.textContent = `${resizePart} · q${txState.quality} · ${txState.mode}${cmpPart}`;
    }
  }
  if (srcSize) srcSize.textContent = txFormatBytes(txState.sourceSize);
  if (cmpSize) {
    if (txState.compressing && txState.compressedBytes == null) {
      cmpSize.textContent = t("tx.compressing");
      cmpSize.classList.remove("tx-stale");
    } else if (txState.compressedBytes != null) {
      const ratio = txState.sourceSize > 0
        ? ` (${(txState.compressedBytes / txState.sourceSize * 100).toFixed(1)}%)`
        : "";
      const staleTag = txState.compressDirty ? t("tx.compress_stale_suffix") : "";
      cmpSize.textContent = `${txFormatBytes(txState.compressedBytes)}${ratio}${staleTag}`;
      cmpSize.classList.toggle("tx-stale", txState.compressDirty);
    } else {
      cmpSize.textContent = "—";
      cmpSize.classList.remove("tx-stale");
    }
  }
}

export function scheduleTxCompress(delayMs = 300) {
  if (txState.compressTimer) clearTimeout(txState.compressTimer);
  txState.compressTimer = setTimeout(() => {
    txState.compressTimer = null;
    runTxCompress();
  }, delayMs);
}

export function getTxFilename() {
  if (!txState.sourceFile) return "image.avif";
  // In file mode, keep the original name (including extension) - that's
  // what the RX expects to decompress and write the final file.
  if (txState.fileMode) {
    return (txState.sourceFile.name || "fichier.bin").slice(0, 60);
  }
  const base = txState.sourceFile.name.replace(/\.[^/.]+$/, "");
  // Envelope allows 64 UTF-8 bytes, leave a little margin.
  return `${base.slice(0, 56)}.avif`;
}

export async function refreshTxEstimate() {
  txState.estimate = null;
  if (!txState.compressedBytes) {
    refreshTxButtons();
    refreshTxPreview();
    return;
  }
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    const est = await invoke("tx_estimate", {
      payloadBytes: txState.compressedBytes,
      mode: txState.mode,
      callsign: txState.resumeCallsign || currentSettings.callsign || "HB9XXX",
      filename: getTxFilename(),
      repairPct: txState.repairPct,
    });
    txState.estimate = est;
  } catch (err) {
    logEvent("tx_estimate_error", { message: String(err) });
  }
  refreshTxButtons();
  refreshTxPreview();
}

export function runTxCompress() {
  // Bail before any state mutation if there's nothing to compress —
  // otherwise showTxBusyOverlay would leave a stuck overlay on an
  // empty preview when the impl exits early on `!sourceFile`.
  if (!txState.sourceFile) return Promise.resolve();
  // Show the overlay synchronously so the user gets immediate feedback,
  // even if `_runTxCompressImpl` only starts 1 microtask later through
  // the `_compressChain`. Without this, clicking "Recalculer" on a fast
  // image (or a small AVIF passthrough) would flash the spinner for a
  // single frame — same root cause as the file-pick path.
  showTxBusyOverlay();
  // Serialize via _compressChain: chain the new compression after the
  // current one, instead of letting ravif run twice in parallel (see
  // _compressChain above).
  const chained = _compressChain
    .then(() => _runTxCompressImpl())
    .catch((err) => logEvent("tx_compress_chain_error", { message: String(err) }));
  _compressChain = chained;
  return chained;
}

export async function _runTxCompressImpl() {
  if (!txState.sourceFile) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const seq = ++txState.compressSeq;
  // A (re)compression prepares a new transmission: zero the RaptorQ block
  // counter so the previous TX's state (kept on screen since 0.15.4) doesn't
  // linger over the new one. Reset BOTH the block grid (lastProgress) AND the
  // fountain status bar (fountainState) — they show the count independently,
  // so clearing only the grid left the bar still reading the old total. A live
  // RX refills them on its next packet.
  setProgressBitmap({ bitmap: null, expected: 0, converged: 0, sigma2: null });
  const v2txt = document.getElementById("v2-progress-text");
  if (v2txt) v2txt.textContent = "—";
  hideFountainStatus();
  drawProgressBlocks();
  // Fresh compressed bytes ⇒ a different RaptorQ session_id (it hashes the
  // payload), so the previous session's ESI high-water + archive ref must NOT
  // carry over — otherwise the next TX continues the OLD fountain (esiMax+1)
  // instead of starting a brand-new session at ESI 0. Resume bypasses this
  // function entirely (it transmits the bit-exact archive without
  // recompressing), so every call here IS a new session.
  clearTxSessionRef();
  txState.compressing = true;
  const previewEl = document.getElementById("tx-preview");
  if (previewEl) previewEl.classList.add("compressing");
  refreshTxPreview();
  refreshTxButtons();
  // Force the browser to paint the loader before launching invoke().
  // Cap the wait with a timeout: on WebKitGTK (Linux) requestAnimationFrame
  // can stall right after a native drag-drop until the next user
  // interaction. Without the fallback, the compression hangs here forever —
  // `compressedBytes` is never set and the TX button stays disabled until
  // the operator clicks "Compresser" again (which generates the frame that
  // unblocks rAF). The timeout lets the compression proceed regardless;
  // worst case the spinner paints one frame late. Same defensive rationale
  // as the 1 s `load`-event fallback in the `finally` below.
  await new Promise((r) => {
    let done = false;
    const go = () => {
      if (done) return;
      done = true;
      r();
    };
    requestAnimationFrame(() => requestAnimationFrame(go));
    setTimeout(go, 100);
  });
  try {
    if (txState.fileMode) {
      logEvent("tx_compress_start", { mode: "zstd", source_len: txState.sourceSize });
      const result = await invoke("compress_file_zstd");
      if (seq !== txState.compressSeq) return; // stale
      txState.compressedBytes = result.byte_len;
      txState.compressedUrl = null;
      txState.compressDirty = false;
      logEvent("tx_compress_done", {
        mode: "zstd",
        source_len: result.source_len,
        byte_len: result.byte_len,
      });
    } else {
      // Defensive resync from the DOM (resize can diverge from txState).
      const checkedRadio = document.querySelector('input[name="tx-resize"]:checked');
      if (checkedRadio && checkedRadio.value !== txState.resize) {
        txState.resize = checkedRadio.value;
      }
      const dims = txTargetDims();
      if (!dims) return;
      logEvent("tx_compress_start", {
        mode: "avif",
        resize: txState.resize,
        target_w: dims.w,
        target_h: dims.h,
        quality: txState.quality,
        speed: txState.speed,
        passthrough: !!txState.avifPassthrough,
      });
      // An active overlay must be baked into the pixels, which requires
      // a real decode/re-encode. We'd love to do that on AVIF sources too,
      // but the `image` crate isn't built with the `avif` feature (no
      // libdav1d dependency), so decoding AVIF bytes Rust-side returns an
      // error — the compression then fails silently and `compressedBytes`
      // stays null, leaving the TX button disabled (regression after the
      // default-overlay seeding). Workaround: keep passthrough ON for AVIF
      // sources even when an overlay is active. The overlay is silently
      // dropped for AVIF inputs (no worse than pre-overlay behaviour);
      // non-AVIF sources still get the overlay baked in normally.
      const ov = getActiveOverlayPayload();
      const result = await invoke("compress_image", {
        opts: {
          target_w: dims.w,
          target_h: dims.h,
          quality: txState.quality,
          speed: txState.speed,
          passthrough: !!txState.avifPassthrough,
          overlay: txState.avifPassthrough ? null : ov,
        },
      });
      if (seq !== txState.compressSeq) return; // stale
      txState.compressedBytes = result.byte_len;
      const url = `${convertFileSrc(result.preview_path)}?v=${Date.now()}`;
      txState.compressedUrl = url;
      txState.compressDirty = false;
      const previewImg = document.getElementById("tx-preview-img");
      if (previewImg) {
        // Explicitly evict the previous decoded image from WebKit's image
        // cache before assigning the new src. Each Recalculer click bumps
        // `?v=Date.now()` so WebKit treats every URL as a brand-new
        // resource and would otherwise accumulate decoded AVIF buffers
        // in the WebProcess across encodes — observed 2026-05-29 :
        // 1-3 successful previews then a Wayland "Broken pipe" / WebKit
        // crash. `removeAttribute("src")` forces WebKit to drop the
        // previous decoded surface before the new fetch starts.
        previewImg.removeAttribute("src");
        previewImg.src = url;
      }
      logEvent("tx_compress_done", {
        mode: "avif",
        source_w: result.source_w,
        source_h: result.source_h,
        actual_w: result.actual_w,
        actual_h: result.actual_h,
        byte_len: result.byte_len,
      });
    }
    refreshTxEstimate();
  } catch (err) {
    if (seq === txState.compressSeq) {
      logEvent("tx_compress_error", { message: String(err) });
    }
  } finally {
    if (seq === txState.compressSeq) {
      txState.compressing = false;
      // Defer the lock release until the new preview image has finished
      // decoding in WebKit. Setting `previewImg.src = url` above only
      // *kicks off* an async fetch+decode in the WebProcess ; if we drop
      // `body.compressing-lock` right here, any clicks that piled up
      // during compression (when pointer-events were blocked from JS but
      // still queued at the WebKit event-pump level) get dispatched
      // simultaneously with libavif rendering — racing into a
      // WebProcess crash on WebKitGTK + libavif (observed 2026-05-29).
      // Waiting on the `load` event gives the decoder a clean window.
      // Falls back to a 1 s safety timer in case the image never fires
      // load/error (e.g. AVIF parse error swallowed silently).
      const release = () => {
        hideTxBusyOverlay();
        refreshTxPreview();
        refreshTxButtons();
      };
      const previewImg = document.getElementById("tx-preview-img");
      if (previewImg && previewImg.getAttribute("src") && !previewImg.complete) {
        let done = false;
        const wrapped = () => { if (!done) { done = true; release(); } };
        previewImg.addEventListener("load", wrapped, { once: true });
        previewImg.addEventListener("error", wrapped, { once: true });
        setTimeout(wrapped, 1000);
      } else {
        release();
      }
    }
  }
}

export function renderFileTape(filename) {
  const container = document.getElementById("tx-file-tape");
  if (!container) return;
  const COL_W = 24;
  const ROW_Y = [16, 38, 60, 88, 110, 132, 154, 176];
  const SPROCKET_Y = 99;
  const TAPE_H = 192;
  // 30 bytes so the scroll loops even for short filenames. If the
  // filename is shorter, repeat it; if longer, truncate it.
  const N_BYTES = 30;
  const src = filename || "fichier.bin";
  const bytes = [];
  for (let i = 0; i < N_BYTES; i++) {
    bytes.push(src.charCodeAt(i % src.length) & 0xff);
  }
  const TAPE_W = N_BYTES * COL_W;
  const holes = [];
  for (let i = 0; i < N_BYTES; i++) {
    const b = bytes[i];
    const x = i * COL_W + COL_W / 2;
    holes.push(`<circle cx="${x}" cy="${SPROCKET_Y}" r="2.5" fill="#1a0e05"/>`);
    for (let bit = 0; bit < 8; bit++) {
      if ((b >> (7 - bit)) & 1) {
        holes.push(`<circle cx="${x}" cy="${ROW_Y[bit]}" r="5.5" fill="#1a0e05"/>`);
      }
    }
  }
  const holesSvg = holes.join("");
  container.innerHTML = `
    <svg viewBox="0 0 ${TAPE_W} ${TAPE_H}" preserveAspectRatio="xMidYMid meet" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <linearGradient id="tape-edge" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stop-color="#5a3a18" stop-opacity="0.55"/>
          <stop offset="0.04" stop-color="#5a3a18" stop-opacity="0"/>
          <stop offset="0.96" stop-color="#5a3a18" stop-opacity="0"/>
          <stop offset="1" stop-color="#5a3a18" stop-opacity="0.55"/>
        </linearGradient>
      </defs>
      <rect x="0" y="0" width="${TAPE_W}" height="${TAPE_H}" fill="#c9a574"/>
      <g>
        ${holesSvg}
        <g transform="translate(${TAPE_W} 0)">${holesSvg}</g>
        <animateTransform attributeName="transform" type="translate"
                          from="0 0" to="${-TAPE_W} 0" dur="9s"
                          repeatCount="indefinite"/>
      </g>
      <rect x="0" y="0" width="${TAPE_W}" height="${TAPE_H}" fill="url(#tape-edge)"/>
    </svg>
  `;
}

export function applyTxModeUI() {
  const passthrough = !!txState.avifPassthrough;
  const file = !!txState.fileMode;
  const lock = passthrough || file;
  const hint = document.getElementById("tx-passthrough-hint");
  if (hint) {
    if (file) {
      hint.hidden = false;
      hint.textContent = t("tx.file_zstd_hint");
    } else if (passthrough) {
      hint.hidden = false;
      hint.textContent = t("tx.passthrough_short");
    } else {
      hint.hidden = true;
    }
  }
  const previewImg = document.getElementById("tx-preview-img");
  if (previewImg) previewImg.style.display = file ? "none" : "";
  const tape = document.getElementById("tx-file-tape");
  if (tape) {
    tape.hidden = !file;
    if (file) {
      const name = (txState.sourceFile && txState.sourceFile.name) || "fichier.bin";
      renderFileTape(name);
    }
  }
  const ids = ["tx-quality", "tx-speed", "tx-free-w", "tx-free-h"];
  for (const id of ids) {
    const el = document.getElementById(id);
    if (el) el.disabled = lock;
  }
  for (const r of document.querySelectorAll('input[name="tx-resize"]')) {
    r.disabled = lock;
  }
}

export function applyPassthroughUI() { applyTxModeUI(); }

export function applyFileModeUI() { applyTxModeUI(); }

export function showTxBusyOverlay() {
  const drop = document.getElementById("tx-drop-zone");
  const preview = document.getElementById("tx-preview");
  if (drop) drop.hidden = true;
  if (preview) {
    preview.hidden = false;
    preview.classList.add("compressing");
  }
  // Block ALL pointer events on the page during compression. Without
  // this, any click anywhere (a tab switch, a settings input, an
  // unrelated button) fired while WebKit is also handling the ravif/
  // libavif decoder for the new preview triggers a WebProcess crash
  // on this distro (observed 2026-05-29 : Wayland "Broken pipe" or
  // "Lost connection to compositor" after exactly one interaction
  // during the encode window). Keyboard still works (lets the user
  // cancel via Ctrl-W / Alt-F4 if they need). Cursor switches to
  // `progress` so it's obvious why clicks are ignored.
  document.body.classList.add("compressing-lock");
}

export function hideTxBusyOverlay() {
  const preview = document.getElementById("tx-preview");
  if (preview) preview.classList.remove("compressing");
  document.body.classList.remove("compressing-lock");
}

export async function loadTxFileFromPath(path) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  // Anti-reentrance: ignore successive drops while a load OR compression
  // is in progress. Without the `compressing` check, dropping a new image
  // during a long ravif encode replaced the backend tx_source mid-flight
  // and piled `_runTxCompressImpl` calls on `_compressChain` until the
  // WebView ran out of memory.
  if (txState.loading || txState.compressing) {
    logEvent("tx_drop_ignored", { message: "loading or compression already in progress", path });
    return;
  }
  txState.loading = true;
  showTxBusyOverlay();
  const url = convertFileSrc(path);
  const name = path.split(/[/\\]/).pop() || "fichier";
  const isImage = isImageFilename(name);
  try {
    // Upload by path (no bytes through IPC).
    const size = await invoke("set_tx_source_from_path", { path });
    if (txState.sourceUrl) {
      URL.revokeObjectURL(txState.sourceUrl);
      txState.sourceUrl = null;
    }
    txState.sourceFile = { name, size };
    txState.sourceSize = size;
    txState.fileMode = !isImage;
    txState.avifPassthrough = isImage && name.toLowerCase().endsWith(".avif");
    txState.compressedBytes = null;
    txState.compressedUrl = null;
    txState.compressDirty = false;
    clearTxSessionRef();
    if (isImage) {
      // Load the image as preview via asset://.
      const img = new Image();
      await new Promise((resolve, reject) => {
        img.onload = () => resolve();
        img.onerror = () => reject(new Error(`image load failed: ${path}`));
        img.src = url;
      });
      txState.sourceImage = img;
      if (txState.resize !== "free") {
        txState.freeW = img.naturalWidth;
        txState.freeH = img.naturalHeight;
        const fw = document.getElementById("tx-free-w");
        const fh = document.getElementById("tx-free-h");
        if (fw) fw.value = txState.freeW;
        if (fh) fh.value = txState.freeH;
      }
    } else {
      txState.sourceImage = null;
    }
    applyPassthroughUI();
    applyFileModeUI();
    document.getElementById("tx-drop-zone").hidden = true;
    const preview = document.getElementById("tx-preview");
    const previewImg = document.getElementById("tx-preview-img");
    if (previewImg) previewImg.src = isImage ? url : "";
    if (preview) preview.hidden = false;
    refreshTxPreview();
    refreshTxButtons();
    scheduleTxCompress(50);
  } catch (err) {
    logEvent("tx_error", { message: `drop ${path}: ${err}` });
    // Compression won't run -- clear the overlay so it doesn't stay stuck.
    hideTxBusyOverlay();
  } finally {
    txState.loading = false;
  }
}

export async function loadTxFile(file) {
  if (!file) return;
  // Anti-reentrance: same rationale as `loadTxFileFromPath` — without
  // this guard, picking a new image during a long ravif encode crashed
  // the WebView via piled-up `_compressChain` impls.
  if (txState.loading || txState.compressing) {
    logEvent("tx_pick_ignored", { message: "loading or compression already in progress" });
    return;
  }
  txState.loading = true;
  showTxBusyOverlay();
  // Release the previous blob URL if any.
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  const isImage = (file.type && file.type.startsWith("image/"))
    || isImageFilename(file.name || "");
  txState.sourceFile = file;
  txState.sourceSize = file.size;
  txState.fileMode = !isImage;
  txState.avifPassthrough = isImage && (
    file.type === "image/avif"
    || (file.name || "").toLowerCase().endsWith(".avif")
  );
  applyPassthroughUI();
  applyFileModeUI();
  txState.compressedBytes = null;
  txState.compressedUrl = null;
  txState.compressDirty = false;
  const url = URL.createObjectURL(file);
  txState.sourceUrl = url;
  const finishLoad = async () => {
    clearTxSessionRef();
    document.getElementById("tx-drop-zone").hidden = true;
    const preview = document.getElementById("tx-preview");
    const previewImg = document.getElementById("tx-preview-img");
    if (previewImg) previewImg.src = isImage ? url : "";
    if (preview) preview.hidden = false;
    refreshTxPreview();
    refreshTxButtons();
    // Upload source to the backend for later compressions.
    try {
      const buf = await file.arrayBuffer();
      await invoke("set_tx_source", { bytes: Array.from(new Uint8Array(buf)) });
      scheduleTxCompress(50);
    } catch (err) {
      logEvent("tx_error", { message: `upload source: ${err}` });
      hideTxBusyOverlay();
    } finally {
      txState.loading = false;
    }
  };
  if (isImage) {
    const img = new Image();
    img.onload = () => {
      txState.sourceImage = img;
      if (txState.resize !== "free") {
        txState.freeW = img.naturalWidth;
        txState.freeH = img.naturalHeight;
        const fw = document.getElementById("tx-free-w");
        const fh = document.getElementById("tx-free-h");
        if (fw) fw.value = txState.freeW;
        if (fh) fh.value = txState.freeH;
      }
      finishLoad();
    };
    img.onerror = () => {
      logEvent("tx_error", { message: `impossible de charger ${file.name}` });
      hideTxBusyOverlay();
      txState.loading = false;
    };
    img.src = url;
  } else {
    txState.sourceImage = null;
    finishLoad();
  }
}

export async function resetTxFile() {
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  txState.sourceFile = null;
  txState.sourceImage = null;
  txState.sourceSize = 0;
  txState.avifPassthrough = false;
  txState.fileMode = false;
  applyPassthroughUI();
  applyFileModeUI();
  txState.compressedBytes = null;
  txState.compressedUrl = null;
  txState.compressDirty = false;
  clearTxSessionRef();
  txState.compressSeq++;
  if (txState.compressTimer) {
    clearTimeout(txState.compressTimer);
    txState.compressTimer = null;
  }
  // Clear busy state too: once reset, the user must be allowed to pick
  // a new file even if an abandoned ravif encode is still finishing on
  // the Rust side. Without this, the `loading || compressing` guard in
  // loadTxFile/loadTxFileFromPath would lock the picker until the
  // discarded compression returned.
  txState.loading = false;
  txState.compressing = false;
  const drop = document.getElementById("tx-drop-zone");
  const preview = document.getElementById("tx-preview");
  const previewImg = document.getElementById("tx-preview-img");
  const fileInput = document.getElementById("tx-file-input");
  if (preview) {
    preview.classList.remove("compressing");
    preview.hidden = true;
  }
  if (drop) drop.hidden = false;
  if (previewImg) previewImg.src = "";
  if (fileInput) fileInput.value = "";
  refreshTxPreview();
  refreshTxButtons();
  try {
    await invoke("clear_tx_source");
  } catch {
    // Doesn't matter: the JS state is already reset.
  }
}

export function setupTxTab() {
  const drop = document.getElementById("tx-drop-zone");
  const fileInput = document.getElementById("tx-file-input");
  if (!drop || !fileInput) return;

  drop.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    const file = fileInput.files && fileInput.files[0];
    // Clear the value first so picking the same file twice still fires
    // a `change` event on the next pick (browsers dedupe on value).
    fileInput.value = "";
    if (file) loadTxFile(file);
  });

  // Drag-drop: on Linux/WebKitGTK the HTML5 dragover/drop events are not
  // reliably surfaced (the WM intercepts). We use the native Tauri v2
  // events (dragDropEnabled:true in tauri.conf.json), emitted at the
  // window level.
  if (window.__TAURI__ && window.__TAURI__.event) {
    const setOver = (on) => drop.classList.toggle("drag-over", on);
    listen("tauri://drag-enter", () => setOver(true)).catch(() => {});
    listen("tauri://drag-over", () => setOver(true)).catch(() => {});
    listen("tauri://drag-leave", () => setOver(false)).catch(() => {});
    listen("tauri://drag-drop", (ev) => {
      setOver(false);
      const paths = (ev && ev.payload && ev.payload.paths) || [];
      if (paths.length > 0) loadTxFileFromPath(paths[0]);
    }).catch(() => {});
  }

  document.getElementById("tx-preview-reset").addEventListener("click", (ev) => {
    ev.stopPropagation();
    resetTxFile();
  });

  document.getElementById("tx-mode").addEventListener("change", (ev) => {
    txState.mode = ev.target.value;
    // New mode -> new session (RaptorQ session_id depends on the mode).
    clearTxSessionRef();
    currentSettings.tx_mode = txState.mode;
    persistSettings();
    refreshTxPreview();
    refreshTxEstimate();
    refreshTxButtons();
    refreshTxExperimentalWarn();
  });

  const markCompressDirty = () => {
    if (txState.compressedBytes != null && !txState.compressDirty) {
      txState.compressDirty = true;
    }
    refreshTxPreview();
    refreshTxButtons();
  };

  const resizeRadios = document.querySelectorAll('input[name="tx-resize"]');
  for (const r of resizeRadios) {
    r.addEventListener("change", () => {
      if (!r.checked) return;
      txState.resize = r.value;
      document.getElementById("tx-resize-free").hidden = r.value !== "free";
      currentSettings.tx_resize = r.value;
      persistSettings();
      markCompressDirty();
    });
  }

  const freeW = document.getElementById("tx-free-w");
  const freeH = document.getElementById("tx-free-h");
  freeW.addEventListener("input", () => {
    const v = parseInt(freeW.value, 10);
    if (!Number.isFinite(v) || v < 1) return;
    txState.freeW = v;
    if (txState.aspectLinked && txState.sourceImage) {
      const ar = txState.sourceImage.naturalHeight / txState.sourceImage.naturalWidth;
      txState.freeH = Math.max(1, Math.round(v * ar));
      freeH.value = txState.freeH;
    }
    markCompressDirty();
  });
  freeH.addEventListener("input", () => {
    const v = parseInt(freeH.value, 10);
    if (!Number.isFinite(v) || v < 1) return;
    txState.freeH = v;
    if (txState.aspectLinked && txState.sourceImage) {
      const ar = txState.sourceImage.naturalWidth / txState.sourceImage.naturalHeight;
      txState.freeW = Math.max(1, Math.round(v * ar));
      freeW.value = txState.freeW;
    }
    markCompressDirty();
  });
  // change (blur/Enter): persist free dimensions without hammering the
  // disk during typing.
  const persistFree = () => {
    currentSettings.tx_free_w = txState.freeW;
    currentSettings.tx_free_h = txState.freeH;
    persistSettings();
  };
  freeW.addEventListener("change", persistFree);
  freeH.addEventListener("change", persistFree);

  const quality = document.getElementById("tx-quality");
  quality.addEventListener("input", () => {
    txState.quality = parseInt(quality.value, 10) || 0;
    document.getElementById("tx-quality-val").textContent = txState.quality;
    markCompressDirty();
  });
  // change = mouseup on the slider: natural moment to persist without
  // hammering the disk during the drag.
  quality.addEventListener("change", () => {
    currentSettings.tx_quality = txState.quality;
    persistSettings();
  });

  const speed = document.getElementById("tx-speed");
  const speedVal = document.getElementById("tx-speed-val");
  const speedHint = document.getElementById("tx-speed-hint");
  const speedLabel = (v) => {
    if (v <= 2) return t("tx.speed_very_slow");
    if (v <= 4) return t("tx.speed_slow");
    if (v <= 6) return t("tx.speed_balanced_2");
    if (v <= 8) return t("tx.speed_fast");
    return t("tx.speed_very_fast");
  };
  speed.value = String(txState.speed);
  speedVal.textContent = String(txState.speed);
  speedHint.textContent = speedLabel(txState.speed);
  speed.addEventListener("input", () => {
    txState.speed = parseInt(speed.value, 10) || 6;
    speedVal.textContent = String(txState.speed);
    speedHint.textContent = speedLabel(txState.speed);
    markCompressDirty();
  });
  speed.addEventListener("change", () => {
    currentSettings.tx_speed = txState.speed;
    persistSettings();
  });

  document.getElementById("tx-btn-compress").addEventListener("click", () => {
    runTxCompress();
  });

  document.getElementById("tx-btn-tx").addEventListener("click", txStart);
  document.getElementById("tx-btn-stop").addEventListener("click", txStop);
  document.getElementById("tx-btn-more").addEventListener("click", txMore);
  const repairPctEl = document.getElementById("tx-repair-pct");
  if (repairPctEl) {
    repairPctEl.value = String(txState.repairPct);
    repairPctEl.addEventListener("change", (ev) => {
      txState.repairPct = parseInt(ev.target.value, 10);
      if (!Number.isFinite(txState.repairPct) || txState.repairPct < 0) {
        txState.repairPct = 5;
      }
      currentSettings.tx_repair_pct = txState.repairPct;
      persistSettings();
      // Refresh estimate: duration and N depend on this %.
      refreshTxEstimate().catch(() => {});
      refreshTxButtons();
    });
  }

  const moreCountEl = document.getElementById("tx-more-count");
  if (moreCountEl) {
    moreCountEl.value = String(txState.moreCount || 5);
    const onMoreChange = () => {
      const v = parseInt(moreCountEl.value, 10);
      if (Number.isFinite(v) && v > 0) txState.moreCount = v;
      refreshTxButtons();
    };
    moreCountEl.addEventListener("input", onMoreChange);
    moreCountEl.addEventListener("change", () => {
      currentSettings.tx_more_count = txState.moreCount;
      persistSettings();
    });
    moreCountEl.addEventListener("change", onMoreChange);
  }
  refreshTxButtons();
}

export let kioskInfoToastTimer = null;

export function showKioskInfoToast(text, durationMs = 5000) {
  if (!document.body.classList.contains("kiosk-mode")) return;
  if (!text) return;
  let el = document.getElementById("kiosk-info-toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "kiosk-info-toast";
    el.className = "kiosk-info-toast";
    document.body.appendChild(el);
  }
  el.textContent = text;
  // Force reflow so the fade-in transition runs again on rapid
  // re-show (e.g. user taps TX twice in a row).
  void el.offsetWidth;
  el.classList.add("show");
  if (kioskInfoToastTimer) clearTimeout(kioskInfoToastTimer);
  kioskInfoToastTimer = setTimeout(() => {
    el.classList.remove("show");
    kioskInfoToastTimer = null;
  }, durationMs);
}

export async function txStart() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  if (txState.txActive) return;
  if (!txState.estimate) {
    logEvent("tx_start_skipped", { reason: "pas d'estimation (compresse d'abord)" });
    return;
  }
  // Kiosk: show a transient info bubble (auto-hides after 5 s) with
  // the same content the desktop hover tooltip carries. The "long
  // transmission" wording uses the same threshold as refreshTxButtons
  // (file vs image mode have distinct warn/hard limits).
  {
    const est = txState.estimate;
    const dur = est.duration_s;
    const warnSeconds = txState.fileMode ? TX_FILE_WARN_SECONDS : TX_WARN_SECONDS;
    showKioskInfoToast(txButtonTitle(est, dur, dur > warnSeconds));
  }
  const rxStopBtn = document.getElementById("btn-stop");
  const rxWasActive = rxStopBtn && !rxStopBtn.disabled;
  // Half-duplex (default): stop RX before TX, then maybeRestartRx() picks
  // it up after tx_complete. Full-duplex (opt-in): leave RX running and
  // never set restartRxAfter so the post-TX path is a no-op.
  const fdx = !!currentSettings.full_duplex_enabled;
  if (rxWasActive && !fdx) {
    try {
      await invoke("stop_capture");
    } catch (err) {
      logEvent("tx_pre_stop_error", { message: String(err) });
    }
  }
  txState.restartRxAfter = rxWasActive && !fdx;
  txState.txActive = true;
  txState.progress = null;
  updateTxProgressText();
  refreshTxButtons();
  logEvent("tx_start", {
    mode: txState.mode,
    callsign: currentSettings.callsign,
    tx_device: currentSettings.tx_device,
    estimate: txState.estimate,
  });
  // The ESI never rewinds. A plain "TX" emits a full initial burst worth of
  // FRESH blocks (n_initial = K + repair) starting at the session's ESI
  // high-water: 0 for a brand-new image (identical to the historical initial
  // burst), or the continuation point for a re-sent / resumed session. This
  // guarantees every TX (and every TX more) adds NEW fountain symbols rather
  // than re-emitting packets recipients already hold.
  const callsign = txState.resumeCallsign || currentSettings.callsign || "";
  const filename = getTxFilename();
  const nInitial = computeNInitial() || 1;
  const prior =
    txState.lastTx && txState.lastTx.mode === txState.mode
      ? txState.lastTx.esiMax + 1
      : 0;
  try {
    if (prior > 0) {
      // Continue the fountain through the existing (OTA-validated) tx_more
      // path — same session_id, fresh ESIs starting at `prior`. `nInitial`
      // is a whole PACKET_QUANTUM, so no extra rounding shifts the high-water.
      logEvent("tx_start_continue", { esi_start: prior, count: nInitial });
      await invoke("tx_more", {
        args: {
          mode: txState.mode,
          callsign,
          filename,
          tx_device: currentSettings.tx_device || "",
          esi_start: prior,
          count: nInitial,
        },
      });
      txState.lastTx = { mode: txState.mode, esiMax: prior + nInitial - 1 };
    } else {
      // Fresh session: tx_start archives the payload and returns the archive
      // path, against which we persist the ESI high-water for later resume.
      const archivePath = await invoke("tx_start", {
        args: {
          mode: txState.mode,
          callsign,
          filename,
          tx_device: currentSettings.tx_device || "",
          repair_pct: txState.repairPct,
        },
      });
      if (archivePath) txState.archivePath = archivePath;
      txState.lastTx = { mode: txState.mode, esiMax: nInitial - 1 };
    }
    await persistNextEsi();
  } catch (err) {
    logEvent("tx_start_error", { message: String(err) });
    txState.txActive = false;
    refreshTxButtons();
    await maybeRestartRx();
  }
}

export function computeK() {
  const est = txState.estimate;
  if (!est) return null;
  if (est.k_source != null) return Math.max(4, est.k_source);
  if (est.total_blocks != null) return Math.max(4, est.total_blocks);
  return null;
}

export function computeNInitial() {
  const est = txState.estimate;
  if (est && est.n_initial != null) return est.n_initial;
  const k = computeK();
  if (!k) return null;
  const pct = txState.repairPct || 0;
  return k + Math.floor((k * pct) / 100);
}

export async function persistNextEsi() {
  if (!txState.archivePath || !txState.lastTx) return;
  try {
    await invoke("tx_set_next_esi", {
      archivePath: txState.archivePath,
      nextEsi: txState.lastTx.esiMax + 1,
    });
  } catch (err) {
    logEvent("tx_next_esi_error", { message: String(err) });
  }
}

export function computeMoreCount() {
  const el = document.getElementById("tx-more-count");
  if (!el) return txState.moreCount || 5;
  const v = parseInt(el.value, 10);
  return Number.isFinite(v) && v > 0 ? v : (txState.moreCount || 5);
}

export async function txMore() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  if (txState.txActive) return;
  if (!txState.lastTx || txState.lastTx.mode !== txState.mode) {
    logEvent("tx_more_skipped", { reason: "pas de TX initial pour ce mode" });
    return;
  }
  const count = computeMoreCount();
  if (!count || count < 1) {
    logEvent("tx_more_skipped", { reason: "count invalide" });
    return;
  }
  const esiStart = txState.lastTx.esiMax + 1;
  const rxStopBtn = document.getElementById("btn-stop");
  const rxWasActive = rxStopBtn && !rxStopBtn.disabled;
  // Same FDX gate as in txStart: keep RX running when full_duplex_enabled
  // is on, otherwise preserve the historical stop-then-restart dance.
  const fdx = !!currentSettings.full_duplex_enabled;
  if (rxWasActive && !fdx) {
    try {
      await invoke("stop_capture");
    } catch (err) {
      logEvent("tx_pre_stop_error", { message: String(err) });
    }
  }
  txState.restartRxAfter = rxWasActive && !fdx;
  txState.txActive = true;
  txState.progress = null;
  // Remember where we'll land after this burst (count packets starting at esiStart).
  txState.lastTx = {
    mode: txState.mode,
    esiMax: esiStart + count - 1,
  };
  updateTxProgressText();
  refreshTxButtons();
  logEvent("tx_more_start", { count, esi_start: esiStart });
  try {
    await invoke("tx_more", {
      args: {
        mode: txState.mode,
        // Reuse the original callsign on a resumed session, else the
        // session_id wouldn't match (see resumeCallsign).
        callsign: txState.resumeCallsign || currentSettings.callsign || "",
        filename: getTxFilename(),
        tx_device: currentSettings.tx_device || "",
        esi_start: esiStart,
        count: count,
      },
    });
    await persistNextEsi();
  } catch (err) {
    logEvent("tx_more_error", { message: String(err) });
    txState.txActive = false;
    refreshTxButtons();
    await maybeRestartRx();
  }
}

export async function txStop() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    await invoke("tx_stop");
  } catch (err) {
    logEvent("tx_stop_error", { message: String(err) });
  }
}

export async function maybeRestartRx() {
  if (!txState.restartRxAfter) return;
  txState.restartRxAfter = false;
  // Small delay to let the TX sound card release its handles before
  // opening the RX capture (especially when the same card is used).
  await new Promise((r) => setTimeout(r, 300));
  await startCapture();
}

export function updateTxProgressText() {
  const txt = document.getElementById("tx-progress-text");
  if (!txt) return;
  const p = txState.progress;
  const est = txState.estimate;
  if (!p) {
    if (est) {
      // K = blocks needed for decoding (RaptorQ source), N = emitted (K + repair).
      // Showing both helps the user understand why the duration goes beyond
      // the strict minimum and how much margin the repair provides.
      const k = est.k_source != null ? est.k_source : est.total_blocks;
      const n = est.n_initial != null ? est.n_initial : est.total_blocks;
      const dur = fmtSeconds(est.duration_s);
      const durK = est.duration_s_k != null ? t("tx.k_threshold_suffix", { dur: fmtSeconds(est.duration_s_k) }) : "";
      txt.textContent = t("tx.fountain_summary", { n, k, dur, durK });
    } else {
      txt.textContent = "—";
    }
    return;
  }
  const kTail = est && est.k_source != null ? ` · K=${est.k_source}` : "";
  txt.textContent = t("tx.progress_blocks", {
    sent: p.blocks_sent,
    total: p.total_blocks,
    tail: kTail,
    elapsed: fmtSeconds(p.elapsed_s),
    dur: fmtSeconds(p.duration_s),
  });
}

export const TX_DUPLEX_VIOLET = { r: 0x5e, g: 0x35, b: 0xb1 }; // #5E35B1 start

export const TX_DUPLEX_BLUE   = { r: 0x29, g: 0xb6, b: 0xf6 }; // #29B6F6 end (logo)

export function lerpDuplexColor(frac) {
  const t = Math.max(0, Math.min(1, frac));
  const r = Math.round(TX_DUPLEX_VIOLET.r + (TX_DUPLEX_BLUE.r - TX_DUPLEX_VIOLET.r) * t);
  const g = Math.round(TX_DUPLEX_VIOLET.g + (TX_DUPLEX_BLUE.g - TX_DUPLEX_VIOLET.g) * t);
  const b = Math.round(TX_DUPLEX_VIOLET.b + (TX_DUPLEX_BLUE.b - TX_DUPLEX_VIOLET.b) * t);
  return `rgb(${r}, ${g}, ${b})`;
}

export function isDuplexActive() {
  return !!currentSettings.full_duplex_enabled
    && rxIsRunning()
    && !!(txState && txState.txActive);
}

export function refreshDuplexTxBar() {
  const bar = document.getElementById("tx-duplex-bar");
  const fill = document.getElementById("tx-duplex-fill");
  if (!bar || !fill) return;
  if (!isDuplexActive() || !txState.progress) {
    bar.hidden = true;
    return;
  }
  const p = txState.progress;
  const total = p.total_blocks || 0;
  const sent = p.blocks_sent || 0;
  const frac = total > 0 ? Math.min(1, sent / total) : 0;
  fill.style.width = `${(frac * 100).toFixed(2)}%`;
  fill.style.backgroundColor = lerpDuplexColor(frac);
  bar.hidden = false;
}

export function onTxProgress(payload) {
  txState.progress = payload;
  updateTxProgressText();
  if (isDuplexActive()) {
    // FDX: leave the bottom canvas to RX, paint TX in the dedicated bar.
    refreshDuplexTxBar();
    return;
  }
  // Half-duplex (or RX not running): reuse the bottom progress bar in TX
  // mode, same behaviour as before.
  const bitmap = new Uint8Array(Math.ceil((payload.total_blocks || 0) / 8));
  for (let i = 0; i < payload.blocks_sent; i++) {
    bitmap[i >> 3] |= 1 << (i & 7);
  }
  setProgressBitmap({
    bitmap,
    expected: payload.total_blocks,
    converged: payload.blocks_sent,
    sigma2: null,
  });
  drawProgressBlocks();
}

export async function onTxComplete(payload) {
  logEvent("tx_complete", payload);
  txState.txActive = false;
  txState.progress = null;
  updateTxProgressText();
  refreshTxButtons();
  refreshDuplexTxBar();
  try {
    await invoke("tx_reset");
  } catch (_) {}
  // Keep the final RX raptor grid + constellation on screen after the
  // transmission ends (loopback / full-duplex): the operator wants to see
  // the last decoded state. The visuals are cleared only when a genuinely
  // new session arrives (session_armed with a different session_id).
  await maybeRestartRx();
}

export async function onTxError(payload) {
  logEvent("tx_error", payload);
  txState.txActive = false;
  txState.progress = null;
  updateTxProgressText();
  refreshTxButtons();
  refreshDuplexTxBar();
  try {
    await invoke("tx_reset");
  } catch (_) {}
  // Preserve the final RX raptor grid + constellation (see onTxComplete).
  await maybeRestartRx();
}

export async function relayHistoryItem(absolutePath) {
  // Bascule sur l'onglet TX puis recharge le fichier comme un drag-drop.
  const txBtn = document.querySelector('.tab-bar .tab[data-tab="tx"]');
  if (txBtn) txBtn.click();
  try {
    await loadTxFileFromPath(absolutePath);
  } catch (err) {
    logEvent("history_relay_error", { path: absolutePath, message: String(err) });
  }
}

export async function resumeTxFromHistory(archivePath) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const txBtn = document.querySelector('.tab-bar .tab[data-tab="tx"]');
  if (txBtn) txBtn.click();
  let info;
  try {
    info = await invoke("tx_resume", { archivePath });
  } catch (err) {
    logEvent("tx_resume_error", { path: archivePath, message: String(err) });
    alert(`Reprise impossible : ${err}`);
    return;
  }
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  // Restore the session into txState. We do NOT recompress: the archive IS
  // the wire payload, and the backend tx_payload_path now points at it.
  txState.mode = info.mode;
  const modeSel = document.getElementById("tx-mode");
  if (modeSel) modeSel.value = info.mode;
  txState.sourceFile = { name: info.filename, size: info.byte_len };
  txState.sourceSize = info.byte_len;
  txState.sourceImage = null;
  txState.fileMode = !info.is_image;
  txState.avifPassthrough = info.is_image; // archived images are AVIF
  txState.compressedBytes = info.byte_len;
  txState.compressedUrl = null;
  txState.compressDirty = false;
  txState.repairPct = Number.isFinite(info.repair_pct) ? info.repair_pct : txState.repairPct;
  // Continuation state: reuse the archived callsign + ESI high-water.
  txState.archivePath = info.archive_path || archivePath;
  txState.resumeCallsign = info.callsign || null;
  txState.lastTx = info.next_esi > 0
    ? { mode: info.mode, esiMax: info.next_esi - 1 }
    : null;
  // UI restore.
  applyPassthroughUI();
  applyFileModeUI();
  refreshTxExperimentalWarn();
  const dropZone = document.getElementById("tx-drop-zone");
  if (dropZone) dropZone.hidden = true;
  const preview = document.getElementById("tx-preview");
  const previewImg = document.getElementById("tx-preview-img");
  if (previewImg) {
    previewImg.removeAttribute("src");
    previewImg.src = info.is_image ? `${convertFileSrc(archivePath)}?v=${Date.now()}` : "";
  }
  if (preview) preview.hidden = false;
  const repairEl = document.getElementById("tx-repair-pct");
  if (repairEl) repairEl.value = String(txState.repairPct);
  refreshTxPreview();
  // The estimate (re)enables the TX button and gives n_initial for the
  // continuation burst; it reads txState.compressedBytes set above.
  await refreshTxEstimate();
  refreshTxButtons();
  logEvent("tx_resumed", {
    session_id: info.session_id,
    next_esi: info.next_esi,
    mode: info.mode,
  });
}
