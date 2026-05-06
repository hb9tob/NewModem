// NBFM Modem GUI — 3-tab layout (RX / TX / Info) with per-block progress and
// live constellation display.

// Mapping aligned with modem-core/src/app_header.rs :: mime
//   0 = BINARY, 1 = TEXT, 2 = IMAGE_AVIF, 3 = IMAGE_JPEG, 4 = IMAGE_PNG,
//   5 = ZSTD (non-image file decompressed RX-side by the Rust worker).
const MIME_TYPES = {
  0: "application/octet-stream",
  1: "text/plain",
  2: "image/avif",
  3: "image/jpeg",
  4: "image/png",
  5: "application/zstd",
};
const MIME_BINARY = 0;
const MIME_TEXT = 1;
const MIME_IMAGE_AVIF = 2;
const MIME_IMAGE_JPEG = 3;
const MIME_IMAGE_PNG = 4;
const MIME_ZSTD = 5;

function mimeToExt(code) {
  return MIME_TYPES[code] || "application/octet-stream";
}

function isImageMime(code) {
  return [MIME_IMAGE_AVIF, MIME_IMAGE_JPEG, MIME_IMAGE_PNG].includes(code);
}

function now() {
  return new Date().toLocaleTimeString();
}

// Event log: we also keep an in-memory buffer so we can serialize and
// push it to the Phase D collector at submission time. Capped at 500
// entries like the DOM list.
const eventLogBuffer = [];

function logEvent(name, data) {
  const tsMs = Date.now();
  eventLogBuffer.push({ ts_ms: tsMs, name, data: data ?? null });
  while (eventLogBuffer.length > 500) eventLogBuffer.shift();

  const log = document.getElementById("event-log");
  if (!log) return;
  const li = document.createElement("li");
  const t = document.createElement("span");
  t.className = "ev-time";
  t.textContent = now();
  const n = document.createElement("span");
  n.className = "ev-name";
  n.textContent = name;
  const body = document.createElement("span");
  body.textContent = data ? JSON.stringify(data) : "";
  li.appendChild(t);
  li.appendChild(n);
  li.appendChild(body);
  log.insertBefore(li, log.firstChild);
  while (log.children.length > 500) log.removeChild(log.lastChild);
}

// ────────────────────────────────────────────────────────────── Tabs
function setupTabs() {
  const tabs = document.querySelectorAll(".tab-bar .tab");
  const panels = document.querySelectorAll(".tab-panel");
  for (const btn of tabs) {
    btn.addEventListener("click", () => {
      const target = btn.dataset.tab;
      for (const b of tabs) b.classList.toggle("active", b === btn);
      for (const p of panels) p.classList.toggle("active", p.id === `tab-${target}`);
      if (target === "rx") {
        redrawAll();
        tryAutoStartCapture().catch((err) => console.error("auto-start RX", err));
      }
      if (target === "sessions") refreshSessions();
      if (target === "history") refreshHistory();
      if (target === "channel") stopRxAndTxForChannelTab();
      if (target === "settings") refreshSettingsRxWarn();
    });
  }
}

// Settings tab: if RX is running, the worker has already opened the RX
// sound card and won't pick up a device change until the next start. We
// do NOT cut RX automatically (the user may just want to consult the
// tab) - we show a banner offering to stop RX and disable the RX device
// select to prevent a phantom change (other fields - pre-emphasis, TX
// device, callsign... - remain editable).
function refreshSettingsRxWarn() {
  const stopBtn = document.getElementById("btn-stop");
  const warn = document.getElementById("settings-rx-warn");
  const stopRxBtn = document.getElementById("settings-stop-rx-btn");
  const rxSel = document.getElementById("rx-device-select");
  const deemph = document.getElementById("rx-deemphasis-enabled");
  if (!warn) return;
  const rxRunning = !!(stopBtn && !stopBtn.disabled);
  warn.hidden = !rxRunning;
  if (stopRxBtn) stopRxBtn.disabled = !rxRunning;
  if (rxSel) rxSel.disabled = rxRunning;
  // De-emphasis is read at start_capture time; mid-RX changes have no
  // effect, so we disable the checkbox while RX is running for clarity.
  if (deemph) deemph.disabled = rxRunning;
}

// Channel tab: we stop RX and TX in progress on entry. The attenuation
// setting applies to the next TX, and an RX running while we twiddle the
// slider would risk being saturated by our own test signal later (phase B).
async function stopRxAndTxForChannelTab() {
  const stopBtn = document.getElementById("btn-stop");
  const txStopBtn = document.getElementById("tx-btn-stop");
  const rxRunning = stopBtn && !stopBtn.disabled;
  const txRunning = txStopBtn && !txStopBtn.disabled;
  if (rxRunning) {
    try { await stopCapture(); } catch (err) {
      logEvent("channel_tab_stop_rx_error", { message: String(err) });
    }
  }
  if (txRunning) {
    try { await txStop(); } catch (err) {
      logEvent("channel_tab_stop_tx_error", { message: String(err) });
    }
  }
}

// ────────────────────────────────────────── Sessions tab (RaptorQ)
// Registry keyed by session_id (hex u32) — merged from :
//  - backend list_sessions command on load / refresh / tab click
//  - real-time session_armed / session_progress / session_decoded events
const sessionRegistry = new Map();

async function refreshSessions() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  try {
    const list = await invoke("list_sessions");
    sessionRegistry.clear();
    for (const meta of list) {
      // The backend doesn't track "received / needed" in meta.json (that
      // requires scanning the blob) — initialise from what we know, and let
      // the next session_progress event fill in the live numbers.
      sessionRegistry.set(meta.session_id, {
        ...meta,
        received: meta.decoded ? meta.k_symbols : 0,
        cap_reached: false,
      });
    }
    renderSessionsTable();
  } catch (err) {
    logEvent("sessions_refresh_error", { message: String(err) });
  }
}

function upsertSession(partial) {
  const id = partial.session_id;
  const prev = sessionRegistry.get(id) || {};
  sessionRegistry.set(id, { ...prev, ...partial });
  renderSessionsTable();
}

function renderSessionsTable() {
  const tbody = document.getElementById("sessions-tbody");
  const countEl = document.getElementById("sessions-count");
  if (!tbody) return;
  const entries = Array.from(sessionRegistry.values()).sort(
    (a, b) => (b.created_at || 0) - (a.created_at || 0)
  );
  countEl.textContent =
    entries.length === 0 ? "0 session" : `${entries.length} session${entries.length > 1 ? "s" : ""}`;
  if (entries.length === 0) {
    tbody.innerHTML = `<tr><td colspan="8" class="sessions-empty">Aucune session.</td></tr>`;
    return;
  }
  tbody.innerHTML = entries.map(renderSessionRow).join("");
  // Wire delete buttons (fresh nodes each render).
  for (const btn of tbody.querySelectorAll(".btn-session-delete")) {
    btn.addEventListener("click", async (ev) => {
      const id = parseInt(ev.currentTarget.dataset.sid, 10);
      if (!Number.isFinite(id)) return;
      if (!confirm(`Supprimer la session ${id.toString(16).padStart(8, "0")} ?`)) {
        return;
      }
      try {
        const { invoke } = window.__TAURI__.core;
        await invoke("delete_session", { sessionId: id });
        sessionRegistry.delete(id);
        renderSessionsTable();
      } catch (err) {
        logEvent("delete_session_error", { message: String(err) });
      }
    });
  }
}

function renderSessionRow(s) {
  const idHex = s.session_id.toString(16).padStart(8, "0");
  const k = s.k_symbols || 0;
  const received = s.received || 0;
  const pct = k > 0 ? Math.min(100, Math.round((received * 100) / k)) : 0;
  const ratio = k > 0 ? received / k : 0;
  let fillClass = "";
  let statusClass = "waiting";
  let statusText = "attente";
  if (s.decoded) {
    fillClass = " done";
    statusClass = "done";
    statusText = "décodé";
  } else if (s.cap_reached) {
    fillClass = " cap-reached";
    statusClass = "cap-reached";
    statusText = "cap 3× atteint";
  } else if (ratio >= 2.0) {
    fillClass = " cap-warn";
    statusClass = "cap-warn";
    statusText = "canal dégradé";
  }
  const filename = s.filename || "—";
  const callsign = s.callsign || "—";
  const profile = s.profile || "—";
  const widthPct = Math.min(100, (received * 100) / Math.max(k, 1));
  return `
    <tr>
      <td class="session-id">${idHex}</td>
      <td>${escapeHtml(callsign)}</td>
      <td>${escapeHtml(filename)}</td>
      <td>${escapeHtml(profile)}</td>
      <td>${received} / ${k}</td>
      <td class="progress-cell">
        <span class="progress-bar-bg"><span class="progress-bar-fill${fillClass}" style="width:${widthPct}%"></span></span>
        <span style="margin-left:8px;color:#888">${pct}%</span>
      </td>
      <td><span class="status-chip ${statusClass}">${statusText}</span></td>
      <td><button class="btn-session-delete" data-sid="${s.session_id}" title="Supprimer le dossier session">✕</button></td>
    </tr>`;
}

function escapeHtml(s) {
  if (s == null) return "";
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// ───────────────────────────────────────────────────── Received-file panel
function showCurrentFile(payload) {
  const info = document.getElementById("current-info");
  const wrap = document.getElementById("current-image-wrap");
  const mime = MIME_TYPES[payload.mime_type] || "application/octet-stream";
  info.innerHTML =
    `<strong>De :</strong> ${payload.callsign || "?"} · ` +
    `<strong>Nom :</strong> ${payload.filename} · ` +
    `<strong>Taille :</strong> ${payload.size} o · ` +
    `<strong>MIME :</strong> ${mime} · ` +
    `<strong>σ² :</strong> ${payload.sigma2.toFixed(4)} · ` +
    `<code>${payload.saved_path}</code>`;
  wrap.innerHTML = "";
  if (isImageMime(payload.mime_type)) {
    const { convertFileSrc } = window.__TAURI__.core;
    const src = convertFileSrc(payload.saved_path);
    const img = document.createElement("img");
    img.src = src;
    img.alt = payload.filename;
    img.dataset.src = src;
    img.addEventListener("dblclick", () => openLightbox(src, payload.filename));
    wrap.appendChild(img);
  }
}

// Open the file explorer on the received (selected) file. Uses
// tauri-plugin-opener, which handles Windows (explorer /select), Linux
// (D-Bus FileManager1, parent xdg-open fallback) and macOS (open -R).
async function revealReceivedFile(savedPath) {
  if (!savedPath) return;
  try {
    const opener = window.__TAURI__ && window.__TAURI__.opener;
    if (opener && typeof opener.revealItemInDir === "function") {
      await opener.revealItemInDir(savedPath);
    } else if (window.__TAURI__ && window.__TAURI__.core) {
      // Fallback through direct invoke if the plugin's global surface is
      // not exposed by withGlobalTauri on this Tauri version.
      await window.__TAURI__.core.invoke("plugin:opener|reveal_item_in_dir", {
        path: savedPath,
      });
    }
  } catch (err) {
    console.error("revealItemInDir", err);
  }
}

// ─────────────────────────────────────────────────── Lightbox (double-click)
// Displays the image in OS fullscreen (Tauri setFullscreen) with wheel or
// keyboard zoom (up to 8x to inspect details) and drag/arrow pan.
const LIGHTBOX_MAX_SCALE = 8;
const lightbox = {
  viewEl: null,
  imgEl: null,
  natW: 0,
  natH: 0,
  minScale: 1,
  maxScale: LIGHTBOX_MAX_SCALE,
  scale: 1,
  tx: 0,
  ty: 0,
  dragging: false,
  lastX: 0,
  lastY: 0,
  wasFullscreen: false,
};

async function setWindowFullscreen(flag) {
  try {
    const win = window.__TAURI__.window.getCurrentWindow();
    await win.setFullscreen(flag);
  } catch (err) {
    console.error("setFullscreen", err);
  }
}

// Tauri setFullscreen resolves before the WebView has propagated the new
// viewport. We wait for either a resize event or a safety timeout.
function waitForResize(prevW, prevH, timeoutMs = 400) {
  return new Promise((resolve) => {
    if (window.innerWidth !== prevW || window.innerHeight !== prevH) {
      return resolve();
    }
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      window.removeEventListener("resize", finish);
      resolve();
    };
    window.addEventListener("resize", finish);
    setTimeout(finish, timeoutMs);
  });
}

async function openLightbox(src, alt) {
  lightbox.viewEl = document.getElementById("image-lightbox");
  lightbox.imgEl = document.getElementById("image-lightbox-img");
  if (!lightbox.viewEl || !lightbox.imgEl) return;
  lightbox.imgEl.alt = alt || "";
  lightbox.imgEl.onload = () => {
    lightbox.natW = lightbox.imgEl.naturalWidth || 1;
    lightbox.natH = lightbox.imgEl.naturalHeight || 1;
    fitLightbox();
  };
  lightbox.imgEl.src = src;
  lightbox.viewEl.hidden = false;
  // OS fullscreen via Tauri: the browser requestFullscreen only fullscreens
  // the WebView inside the window, not the window itself.
  try {
    const win = window.__TAURI__.window.getCurrentWindow();
    lightbox.wasFullscreen = await win.isFullscreen();
    if (!lightbox.wasFullscreen) {
      const prevW = window.innerWidth;
      const prevH = window.innerHeight;
      await win.setFullscreen(true);
      // Wait for the resize to propagate WebView-side before fitting,
      // otherwise we compute the center with the windowed dimensions and
      // the image appears offset toward the top-left corner.
      await waitForResize(prevW, prevH);
    }
  } catch (err) {
    console.error("isFullscreen/setFullscreen", err);
  }
  // If the image is cached, onload may not refire - explicit refit with the
  // final viewport size.
  if (lightbox.imgEl.complete && lightbox.imgEl.naturalWidth > 0) {
    lightbox.natW = lightbox.imgEl.naturalWidth;
    lightbox.natH = lightbox.imgEl.naturalHeight;
  }
  fitLightbox();
}

async function closeLightbox() {
  if (!lightbox.viewEl) return;
  lightbox.viewEl.hidden = true;
  lightbox.imgEl.src = "";
  if (!lightbox.wasFullscreen) {
    await setWindowFullscreen(false);
  }
}

// Keeps the image at least partially inside the viewport:
//  - if it fits entirely (w <= vw / h <= vh), we center it;
//  - otherwise, we prevent it from sliding off-screen (at minimum one edge
//    touches an edge of the viewport).
function clampLightboxPan() {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const w = lightbox.natW * lightbox.scale;
  const h = lightbox.natH * lightbox.scale;
  if (w <= vw) {
    lightbox.tx = (vw - w) / 2;
  } else {
    lightbox.tx = Math.max(vw - w, Math.min(0, lightbox.tx));
  }
  if (h <= vh) {
    lightbox.ty = (vh - h) / 2;
  } else {
    lightbox.ty = Math.max(vh - h, Math.min(0, lightbox.ty));
  }
}

function applyLightboxTransform() {
  if (!lightbox.imgEl) return;
  clampLightboxPan();
  const { imgEl, scale, tx, ty } = lightbox;
  imgEl.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
}

function fitLightbox() {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  // fit = what makes the whole image fit in the viewport, capped at 1:1
  // (no auto-upscale for small images).
  const fit = Math.min(vw / lightbox.natW, vh / lightbox.natH, 1);
  lightbox.minScale = fit;
  lightbox.maxScale = LIGHTBOX_MAX_SCALE;
  lightbox.scale = fit;
  lightbox.tx = (vw - lightbox.natW * fit) / 2;
  lightbox.ty = (vh - lightbox.natH * fit) / 2;
  applyLightboxTransform();
}

function zoomLightboxBy(factor, cx, cy) {
  const prev = lightbox.scale;
  let next = prev * factor;
  next = Math.max(lightbox.minScale, Math.min(lightbox.maxScale, next));
  if (next === prev) return;
  // Zoom centered on (cx, cy): this point in screen coords stays fixed.
  lightbox.tx = cx - (cx - lightbox.tx) * (next / prev);
  lightbox.ty = cy - (cy - lightbox.ty) * (next / prev);
  lightbox.scale = next;
  applyLightboxTransform();
}

function zoomLightbox(delta, cx, cy) {
  zoomLightboxBy(Math.exp(-delta * 0.0015), cx, cy);
}

function panLightbox(dx, dy) {
  lightbox.tx += dx;
  lightbox.ty += dy;
  applyLightboxTransform();
}

function setupLightbox() {
  const view = document.getElementById("image-lightbox");
  if (!view) return;
  view.addEventListener("wheel", (ev) => {
    if (view.hidden) return;
    ev.preventDefault();
    zoomLightbox(ev.deltaY, ev.clientX, ev.clientY);
  }, { passive: false });
  view.addEventListener("mousedown", (ev) => {
    if (view.hidden) return;
    lightbox.dragging = true;
    lightbox.lastX = ev.clientX;
    lightbox.lastY = ev.clientY;
    view.classList.add("dragging");
  });
  window.addEventListener("mousemove", (ev) => {
    if (!lightbox.dragging) return;
    lightbox.tx += ev.clientX - lightbox.lastX;
    lightbox.ty += ev.clientY - lightbox.lastY;
    lightbox.lastX = ev.clientX;
    lightbox.lastY = ev.clientY;
    applyLightboxTransform();
  });
  window.addEventListener("mouseup", () => {
    if (!lightbox.dragging) return;
    lightbox.dragging = false;
    view.classList.remove("dragging");
  });
  // Single click on the background (not the image) closes. Double-click also closes.
  view.addEventListener("click", (ev) => {
    if (ev.target === view) closeLightbox();
  });
  view.addEventListener("dblclick", closeLightbox);
  window.addEventListener("keydown", (ev) => {
    if (view.hidden) return;
    const cx = window.innerWidth / 2;
    const cy = window.innerHeight / 2;
    const PAN_STEP = 60;
    // We look at both key AND code: on some layouts (Swiss AZERTY), the
    // numpad does not surface "+"/"-" as key, but NumpadAdd/Subtract is
    // always there.
    const isPlus = ev.key === "+" || ev.key === "=" || ev.key === "a" || ev.key === "A" || ev.code === "NumpadAdd";
    const isMinus = ev.key === "-" || ev.key === "_" || ev.key === "q" || ev.key === "Q" || ev.code === "NumpadSubtract";
    const isZero = ev.key === "0" || ev.code === "Numpad0";
    if (ev.key === "Escape") {
      closeLightbox();
    } else if (isPlus) {
      zoomLightboxBy(1.25, cx, cy);
      ev.preventDefault();
    } else if (isMinus) {
      zoomLightboxBy(1 / 1.25, cx, cy);
      ev.preventDefault();
    } else if (isZero) {
      fitLightbox();
      ev.preventDefault();
    } else if (ev.key === "ArrowLeft") {
      panLightbox(PAN_STEP, 0);
      ev.preventDefault();
    } else if (ev.key === "ArrowRight") {
      panLightbox(-PAN_STEP, 0);
      ev.preventDefault();
    } else if (ev.key === "ArrowUp") {
      panLightbox(0, PAN_STEP);
      ev.preventDefault();
    } else if (ev.key === "ArrowDown") {
      panLightbox(0, -PAN_STEP);
      ev.preventDefault();
    }
  });
  // Resize fires after Tauri setFullscreen has resized the window, which
  // re-centers the image in the new viewport.
  window.addEventListener("resize", () => {
    if (!view.hidden) fitLightbox();
  });
}

// ─────────────────────────────────────────── Settings / device selection
// Both sound cards (RX/TX) + the callsign live in the Settings tab and are
// persisted via the Tauri get_settings / save_settings commands.
let currentSettings = {
  callsign: "",
  rx_device: "",
  tx_device: "",
  ptt_enabled: false,
  ptt_port: "",
  ptt_use_rts: true,
  ptt_use_dtr: false,
  ptt_rts_tx_high: true,
  ptt_dtr_tx_high: true,
  tx_attenuation_db: 0,
  tx_preemphasis_enabled: false,
  rx_deemphasis_enabled: false,
  collector_url: "",
  tx_quality: 10,
  tx_repair_pct: 5,
  tx_mode: "HIGH56",
  tx_resize: "800x600",
  tx_free_w: 800,
  tx_free_h: 600,
  tx_speed: 6,
  tx_more_count: 5,
  /// If true, the RX profile is locked on rx_forced_profile and auto-
  /// detection is disabled. Required to receive MEGA, FAST, HIGH++ or
  /// HIGH+56 (which are outside PROBE_TEMPLATES).
  rx_force_mode: false,
  rx_forced_profile: "HIGH56",
  /// If true, experimental profiles appear in the TX and RX combos and
  /// the "Force a profile" UI becomes visible at startup. If false
  /// (default): combos are filtered to standard profiles (ULTRA /
  /// ROBUST / NORMAL / HIGH / HIGH56 / HIGH+) and the rx-force-bar is
  /// hidden.
  experimental_modes_enabled: false,
  overlays: [],
  active_overlay: 0,
};

// Default empty overlay slots (mirrors `default_overlay_slots()` on the
// Rust side). Slot 0 is the immutable "Aucun" entry.
function makeDefaultOverlays() {
  return [
    { name: "Aucun", text: null, logo: null },
    { name: "Overlay 1", text: null, logo: null },
    { name: "Overlay 2", text: null, logo: null },
    { name: "Overlay 3", text: null, logo: null },
    { name: "Overlay 4", text: null, logo: null },
  ];
}

function ensureOverlaySlots() {
  if (!Array.isArray(currentSettings.overlays) || currentSettings.overlays.length === 0) {
    currentSettings.overlays = makeDefaultOverlays();
  }
  while (currentSettings.overlays.length < 5) {
    const i = currentSettings.overlays.length;
    currentSettings.overlays.push({ name: i === 0 ? "Aucun" : `Overlay ${i}`, text: null, logo: null });
  }
  if (typeof currentSettings.active_overlay !== "number"
      || currentSettings.active_overlay < 0
      || currentSettings.active_overlay >= currentSettings.overlays.length) {
    currentSettings.active_overlay = 0;
  }
}

function populateDeviceSelect(selectId, devices, savedName, plutoDevices) {
  const select = document.getElementById(selectId);
  if (!select) return null;
  select.innerHTML = "";
  const audio = devices || [];
  const pluto = plutoDevices || [];
  if (audio.length === 0 && pluto.length === 0) {
    const opt = document.createElement("option");
    opt.textContent = "aucun périphérique détecté";
    opt.value = "";
    select.appendChild(opt);
    return null;
  }
  let preferred = null;
  for (const dev of audio) {
    const opt = document.createElement("option");
    opt.value = dev.name;
    const range = dev.max_sample_rate > 0
      ? `${dev.min_sample_rate}–${dev.max_sample_rate} Hz`
      : "?";
    const tag48 = dev.supports_48k ? " ✓48k" : "";
    const tagDef = dev.is_default ? " [default]" : "";
    const tagErr = dev.error ? ` [${dev.error}]` : "";
    opt.textContent = `${dev.friendly_name} — ${range}${tag48}${tagDef}${tagErr}`;
    opt.dataset.supports48k = dev.supports_48k ? "1" : "0";
    opt.dataset.kind = "audio";
    select.appendChild(opt);
    if (preferred === null && dev.supports_48k) preferred = dev;
    if (dev.is_default && dev.supports_48k) preferred = dev;
  }
  // Pluto entries — synthetic device names like `pluto:usb:1.6.5`. The
  // backend's start_capture routes on the prefix and opens libiio
  // instead of cpal. Pluto entries always claim 48 kHz support
  // because the modem-pluto chain decimates the AD9361 IF down to
  // exactly 48 kHz before the worker sees a single sample.
  for (const dev of pluto) {
    const opt = document.createElement("option");
    opt.value = dev.name;
    opt.textContent = `${dev.friendly_name} ✓48k [SDR]`;
    opt.dataset.supports48k = "1";
    opt.dataset.kind = "pluto";
    select.appendChild(opt);
  }
  // Priority: saved value if still available, otherwise the preferred one.
  if (savedName && (audio.some(d => d.name === savedName)
      || pluto.some(d => d.name === savedName))) {
    select.value = savedName;
  } else if (preferred) {
    select.value = preferred.name;
  }
  return select.value || null;
}

/// Show or hide the Pluto-specific settings sub-panels based on
/// whether the currently-selected RX/TX device is a Pluto. Called
/// whenever rx/tx-device-select changes and at startup. Each
/// direction has its own `pluto-*-config` block now (no more single
/// shared `pluto-config` fieldset).
function refreshPlutoPanelVisibility() {
  const refresh = (selectId, panelId) => {
    const sel = document.getElementById(selectId);
    const panel = document.getElementById(panelId);
    if (!sel || !panel) return;
    const opt = sel.options[sel.selectedIndex];
    const isPluto = !!(opt && opt.dataset && opt.dataset.kind === "pluto");
    panel.hidden = !isPluto;
  };
  refresh("rx-device-select", "pluto-rx-config");
  refresh("tx-device-select", "pluto-tx-config");
}

/// EIA standard CTCSS tones (39 values, in Hz). Mirror of
/// `modem_sdr_dsp::ctcss_gen::EIA_CTCSS_TONES_HZ`. Order matches
/// repeater documentation conventions (low → high).
const EIA_CTCSS_TONES_HZ = [
  67.0, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8,
  97.4, 100.0, 103.5, 107.2, 110.9, 114.8, 118.8, 123.0, 127.3, 131.8,
  136.5, 141.3, 146.2, 151.4, 156.7, 162.2, 167.9, 173.8, 179.9, 186.2,
  192.8, 203.5, 210.7, 218.1, 225.7, 233.6, 241.8, 250.3, 254.1,
];

/// Populate the CTCSS frequency dropdown with all 39 EIA tones, once
/// at GUI init. The currently-saved value is selected on the next
/// loadSettings() pass.
function populateCtcssDropdown() {
  const sel = document.getElementById("pluto-tx-ctcss-freq");
  if (!sel || sel.children.length > 0) return;
  for (const f of EIA_CTCSS_TONES_HZ) {
    const opt = document.createElement("option");
    opt.value = String(f);
    opt.textContent = `${f.toFixed(1)} Hz`;
    sel.appendChild(opt);
  }
}

/// Recompute and display the effective TX deviation = preset + offset,
/// as both the visible "5.0 kHz" label and the slider's tooltip.
/// Called from radio/slider change handlers.
function refreshPlutoTxDeviationLabel() {
  const presetEl = document.querySelector('input[name="pluto-tx-dev-preset"]:checked');
  const offsetEl = document.getElementById("pluto-tx-dev-offset");
  const out = document.getElementById("pluto-tx-dev-effective");
  if (!presetEl || !offsetEl || !out) return;
  const preset = parseInt(presetEl.value, 10) || 5000;
  const offset = parseInt(offsetEl.value, 10) || 0;
  const eff = Math.max(500, Math.min(8000, preset + offset));
  out.textContent = (eff / 1000).toFixed(1) + " kHz";
}

function refreshRxDeviceLabel() {
  const label = document.getElementById("rx-device-label");
  const select = document.getElementById("rx-device-select");
  if (!label || !select) return;
  const opt = select.options[select.selectedIndex];
  label.textContent = opt && opt.value ? opt.textContent : "— aucune carte RX";
}

function refreshStartButtonFromRx() {
  const select = document.getElementById("rx-device-select");
  const btn = document.getElementById("btn-start");
  if (!select || !btn) return;
  const opt = select.options[select.selectedIndex];
  const ok = !!(opt && opt.value && opt.dataset.supports48k === "1");
  // Don't touch btn-start if capture is running (disabled via startCapture).
  if (!document.getElementById("btn-stop").disabled) return;
  btn.disabled = !ok;
}

async function loadDevices() {
  const status = document.getElementById("status");
  if (!window.__TAURI__ || !window.__TAURI__.core) {
    status.textContent = "API Tauri indisponible";
    status.style.color = "#ef5350";
    return;
  }
  const { invoke } = window.__TAURI__.core;
  try {
    // Pluto enumeration is best-effort: libiio may not find any Plutos
    // (none plugged in / network mode disabled) — that surfaces as an
    // empty list, not an error, so the dropdown still loads cleanly.
    const [rxDevices, txDevices, plutoDevices] = await Promise.all([
      invoke("list_audio_devices"),
      invoke("list_output_audio_devices"),
      invoke("list_pluto_devices").catch(err => {
        console.warn("list_pluto_devices failed:", err);
        return [];
      }),
    ]);
    populateDeviceSelect("rx-device-select", rxDevices, currentSettings.rx_device, plutoDevices);
    populateDeviceSelect("tx-device-select", txDevices, currentSettings.tx_device, plutoDevices);
    const n48 = rxDevices.filter(d => d.supports_48k).length;
    const plutoTag = plutoDevices.length > 0 ? ` · ${plutoDevices.length} Pluto` : "";
    status.textContent = `${rxDevices.length} RX (${n48} @48k) · ${txDevices.length} TX${plutoTag}`;
    refreshRxDeviceLabel();
    refreshStartButtonFromRx();
    refreshPlutoPanelVisibility();
  } catch (err) {
    status.textContent = `erreur : ${err}`;
    status.style.color = "#ef5350";
  }
}

async function loadSettings() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  try {
    currentSettings = await invoke("get_settings");
  } catch (err) {
    console.error("get_settings", err);
    currentSettings = {
      callsign: "", rx_device: "", tx_device: "",
      ptt_enabled: false, ptt_port: "",
      ptt_use_rts: true, ptt_use_dtr: false,
      ptt_rts_tx_high: true, ptt_dtr_tx_high: true,
      tx_attenuation_db: 0, tx_preemphasis_enabled: false, rx_deemphasis_enabled: false, collector_url: "",
      tx_quality: 10, tx_repair_pct: 5,
      tx_mode: "HIGH", tx_resize: "800x600",
      tx_free_w: 800, tx_free_h: 600,
      tx_speed: 6, tx_more_count: 5,
      tx_history_max: 100,
      overlays: makeDefaultOverlays(), active_overlay: 0,
    };
  }
  ensureOverlaySlots();
  const call = document.getElementById("callsign-input");
  if (call) call.value = currentSettings.callsign || "";
  // Fetch profile list from modem-core BEFORE any code that touches the
  // tx-mode / rx-forced-profile selects (they're empty in index.html).
  await loadModemProfiles();
  applyPttSettingsToUI();
  applyRxForceSettingsToUI();
  applyExperimentalModesToUI();
  const colUrl = document.getElementById("collector-url");
  if (colUrl) colUrl.value = currentSettings.collector_url || "";
  const histMax = document.getElementById("tx-history-max-input");
  if (histMax) histMax.value = String(currentSettings.tx_history_max ?? 100);
  const preemph = document.getElementById("tx-preemphasis-enabled");
  if (preemph) preemph.checked = !!currentSettings.tx_preemphasis_enabled;
  const deemph = document.getElementById("rx-deemphasis-enabled");
  if (deemph) deemph.checked = !!currentSettings.rx_deemphasis_enabled;

  // Pluto-specific knobs — populated whenever Settings load. Frequencies
  // are stored in Hz on the Rust side and shown as MHz to the user.
  const plutoRxFreq = document.getElementById("pluto-rx-freq-mhz");
  if (plutoRxFreq) plutoRxFreq.value =
    ((currentSettings.pluto_rx_freq_hz ?? 145_500_000) / 1e6).toFixed(3);
  const plutoTxFreq = document.getElementById("pluto-tx-freq-mhz");
  if (plutoTxFreq) plutoTxFreq.value =
    ((currentSettings.pluto_tx_freq_hz ?? 145_500_000) / 1e6).toFixed(3);
  const plutoRxMode = document.getElementById("pluto-rx-gain-mode");
  if (plutoRxMode) plutoRxMode.value =
    currentSettings.pluto_rx_gain_mode || "slow_attack";
  const plutoRxGain = document.getElementById("pluto-rx-gain-db");
  if (plutoRxGain) plutoRxGain.value = String(currentSettings.pluto_rx_gain_db ?? 30);
  const plutoTxAtt = document.getElementById("pluto-tx-att-db");
  if (plutoTxAtt) plutoTxAtt.value = String(currentSettings.pluto_tx_attenuation_db ?? 30);
  // FM deviation : RX preset (5/2.5 kHz radio), TX preset (radio) +
  // fine-tune offset (slider), and the live "X.X kHz" effective label.
  const rxDev = currentSettings.pluto_rx_deviation_hz ?? 5000;
  const rxRadio = document.querySelector(`input[name="pluto-rx-dev"][value="${rxDev}"]`);
  if (rxRadio) rxRadio.checked = true;
  const txPreset = currentSettings.pluto_tx_deviation_preset_hz ?? 5000;
  const txPresetRadio = document.querySelector(`input[name="pluto-tx-dev-preset"][value="${txPreset}"]`);
  if (txPresetRadio) txPresetRadio.checked = true;
  const txOffset = currentSettings.pluto_tx_deviation_offset_hz ?? 0;
  const txOffsetSlider = document.getElementById("pluto-tx-dev-offset");
  if (txOffsetSlider) txOffsetSlider.value = String(txOffset);
  refreshPlutoTxDeviationLabel();
  // CTCSS : enable checkbox + tone dropdown.
  const ctcssEn = document.getElementById("pluto-tx-ctcss-enabled");
  if (ctcssEn) ctcssEn.checked = !!currentSettings.pluto_tx_ctcss_enabled;
  const ctcssFreq = document.getElementById("pluto-tx-ctcss-freq");
  if (ctcssFreq) {
    const f = currentSettings.pluto_tx_ctcss_freq_hz ?? 88.5;
    // Snap to nearest EIA tone if the saved value drifted (e.g. user
    // hand-edited settings.json with a non-standard frequency).
    const closest = EIA_CTCSS_TONES_HZ.reduce(
      (best, t) => (Math.abs(t - f) < Math.abs(best - f) ? t : best),
      EIA_CTCSS_TONES_HZ[0]
    );
    ctcssFreq.value = String(closest);
  }
  applyTxSettingsToUI();
}

function applyRxForceSettingsToUI() {
  const cb = document.getElementById("rx-force-mode");
  const sel = document.getElementById("rx-forced-profile");
  if (cb) cb.checked = !!currentSettings.rx_force_mode;
  if (sel) {
    sel.value = currentSettings.rx_forced_profile || "HIGH56";
    sel.disabled = !currentSettings.rx_force_mode;
  }
}

/// Profiles fetched from modem-core via the Tauri command list_modem_profiles.
/// Drives the contents of the tx-mode and rx-forced-profile combos so the GUI
/// never hard-codes the modem list — adding a profile in modem-core makes it
/// appear here with no JS/HTML change required.
let modemProfiles = [];

async function loadModemProfiles() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  try {
    modemProfiles = await invoke("list_modem_profiles");
  } catch (err) {
    console.error("list_modem_profiles", err);
    modemProfiles = [];
  }
  populateProfileSelects();
}

/// (Re)builds the tx-mode and rx-forced-profile <select>s from the cached
/// profile descriptors. When the experimental toggle is OFF, profiles flagged
/// experimental are physically excluded — hiding via `hidden` is unreliable
/// across some Tauri WebViews. The previous select.value is preserved when
/// the matching option is still present.
function populateProfileSelects() {
  const allowExp = !!currentSettings.experimental_modes_enabled;
  populateOneProfileSelect("tx-mode", allowExp, /*rich=*/true);
  populateOneProfileSelect("rx-forced-profile", allowExp, /*rich=*/false);
}

function populateOneProfileSelect(selId, allowExperimental, rich) {
  const sel = document.getElementById(selId);
  if (!sel) return;
  const prev = sel.value;
  sel.innerHTML = "";
  for (const p of modemProfiles) {
    if (!allowExperimental && p.experimental) continue;
    const opt = document.createElement("option");
    opt.value = p.name;
    if (rich) {
      opt.textContent = p.experimental
        ? `⚠ ${p.label} [EXPÉRIMENTAL]`
        : p.label;
    } else {
      opt.textContent = p.experimental
        ? `⚠ ${p.name} [EXPÉRIMENTAL]`
        : p.name;
    }
    if (p.experimental) opt.classList.add("experimental-option");
    sel.appendChild(opt);
  }
  if (prev && sel.querySelector(`option[value="${CSS.escape(prev)}"]`)) {
    sel.value = prev;
  } else if (sel.options.length > 0) {
    sel.value = sel.options[0].value;
  }
}

function experimentalProfileNames() {
  return modemProfiles.filter((p) => p.experimental).map((p) => p.name);
}

/// Apply the state of the "Enable experimental modes" toggle:
/// - update the settings checkbox
/// - re-populate the profile combos with experimentals filtered in/out
/// - hide/show the "Force a profile" bar of the RX tab
/// If the user disables the toggle while rx_force_mode is ON, we disable
/// forced mode to avoid staying locked on an experimental profile with no
/// way to reach it. Same if the current persisted profile is experimental:
/// we fall back to HIGH56 (standard since 2026-04-28).
function applyExperimentalModesToUI() {
  const enabled = !!currentSettings.experimental_modes_enabled;
  const cb = document.getElementById("experimental-modes-enabled");
  if (cb) cb.checked = enabled;

  populateProfileSelects();

  const forceBar = document.getElementById("rx-force-bar");
  if (forceBar) forceBar.hidden = !enabled;

  const expNames = experimentalProfileNames();
  let needPersist = false;
  if (!enabled) {
    if (expNames.includes(currentSettings.tx_mode)) {
      currentSettings.tx_mode = "HIGH56";
      needPersist = true;
    }
    if (expNames.includes(currentSettings.rx_forced_profile)) {
      currentSettings.rx_forced_profile = "HIGH56";
      needPersist = true;
    }
    if (currentSettings.rx_force_mode) {
      currentSettings.rx_force_mode = false;
      const forceCb = document.getElementById("rx-force-mode");
      const forceSel = document.getElementById("rx-forced-profile");
      if (forceCb) forceCb.checked = false;
      if (forceSel) forceSel.disabled = true;
      needPersist = true;
    }
  }
  if (needPersist) persistSettings();
}

// Sync all persisted TX settings into txState and the UI. Called after
// loadSettings, so setupTxTab has already attached its listeners - we just
// update the values.
function applyTxSettingsToUI() {
  const intOr = (v, def) => Number.isFinite(v) ? v : def;
  const q = intOr(currentSettings.tx_quality, 10);
  const r = intOr(currentSettings.tx_repair_pct, 5);
  const sp = intOr(currentSettings.tx_speed, 6);
  const mc = intOr(currentSettings.tx_more_count, 5);
  const fw = intOr(currentSettings.tx_free_w, 800);
  const fh = intOr(currentSettings.tx_free_h, 600);
  const mode = currentSettings.tx_mode || "HIGH56";
  const resize = currentSettings.tx_resize || "800x600";

  txState.quality = q;
  txState.repairPct = r;
  txState.speed = sp;
  txState.moreCount = mc;
  txState.freeW = fw;
  txState.freeH = fh;
  txState.mode = mode;
  txState.resize = resize;

  const setVal = (id, v) => {
    const el = document.getElementById(id);
    if (el) el.value = String(v);
  };
  const setText = (id, v) => {
    const el = document.getElementById(id);
    if (el) el.textContent = String(v);
  };

  setVal("tx-quality", q);
  setText("tx-quality-val", q);
  setVal("tx-speed", sp);
  setText("tx-speed-val", sp);
  setVal("tx-repair-pct", r);
  setVal("tx-more-count", mc);
  setVal("tx-free-w", fw);
  setVal("tx-free-h", fh);

  const modeSel = document.getElementById("tx-mode");
  if (modeSel) modeSel.value = mode;

  for (const radio of document.querySelectorAll('input[name="tx-resize"]')) {
    radio.checked = radio.value === resize;
  }
  const freeWrap = document.getElementById("tx-resize-free");
  if (freeWrap) freeWrap.hidden = resize !== "free";

  refreshTxExperimentalWarn();
}

function applyPttSettingsToUI() {
  const en = document.getElementById("ptt-enabled");
  const cfg = document.getElementById("ptt-config");
  const rts = document.getElementById("ptt-use-rts");
  const dtr = document.getElementById("ptt-use-dtr");
  if (en) en.checked = !!currentSettings.ptt_enabled;
  if (cfg) cfg.hidden = !currentSettings.ptt_enabled;
  if (rts) rts.checked = !!currentSettings.ptt_use_rts;
  if (dtr) dtr.checked = !!currentSettings.ptt_use_dtr;
  const rtsPol = currentSettings.ptt_rts_tx_high ? "high" : "low";
  const dtrPol = currentSettings.ptt_dtr_tx_high ? "high" : "low";
  document.querySelectorAll('input[name="ptt-rts-pol"]').forEach(r => {
    r.checked = (r.value === rtsPol);
  });
  document.querySelectorAll('input[name="ptt-dtr-pol"]').forEach(r => {
    r.checked = (r.value === dtrPol);
  });
}

async function loadSerialPorts() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  const sel = document.getElementById("ptt-port-select");
  if (!sel) return;
  let ports = [];
  try {
    ports = await invoke("list_serial_ports");
  } catch (err) {
    console.error("list_serial_ports", err);
  }
  const saved = currentSettings.ptt_port || "";
  sel.innerHTML = "";
  if (ports.length === 0) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "— aucun port détecté —";
    sel.appendChild(opt);
  } else {
    if (saved && !ports.includes(saved)) {
      // Keep the saved value even when absent, to make it visible.
      const opt = document.createElement("option");
      opt.value = saved;
      opt.textContent = `${saved} (introuvable)`;
      sel.appendChild(opt);
    }
    for (const name of ports) {
      const opt = document.createElement("option");
      opt.value = name;
      opt.textContent = name;
      sel.appendChild(opt);
    }
    sel.value = saved || ports[0];
  }
}

function readPttFormIntoSettings() {
  const en = document.getElementById("ptt-enabled");
  const port = document.getElementById("ptt-port-select");
  const rts = document.getElementById("ptt-use-rts");
  const dtr = document.getElementById("ptt-use-dtr");
  const rtsHigh = document.querySelector('input[name="ptt-rts-pol"]:checked');
  const dtrHigh = document.querySelector('input[name="ptt-dtr-pol"]:checked');
  currentSettings.ptt_enabled = !!(en && en.checked);
  currentSettings.ptt_port = port ? (port.value || "") : "";
  currentSettings.ptt_use_rts = !!(rts && rts.checked);
  currentSettings.ptt_use_dtr = !!(dtr && dtr.checked);
  currentSettings.ptt_rts_tx_high = !rtsHigh || rtsHigh.value === "high";
  currentSettings.ptt_dtr_tx_high = !dtrHigh || dtrHigh.value === "high";
}

function renderPttStatus(payload) {
  const el = document.getElementById("ptt-status");
  if (!el) return;
  const state = (payload && payload.state) || "off";
  const msg = (payload && payload.message) || "—";
  el.classList.remove("ok", "error", "off");
  el.classList.add(state);
  el.textContent = msg;
}

async function persistSettings() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  const call = document.getElementById("callsign-input");
  const rxSel = document.getElementById("rx-device-select");
  const txSel = document.getElementById("tx-device-select");
  readPttFormIntoSettings();
  const colUrl = document.getElementById("collector-url");
  const histMax = document.getElementById("tx-history-max-input");
  currentSettings.callsign = (call && call.value || "").trim().toUpperCase();
  currentSettings.rx_device = rxSel ? rxSel.value || "" : "";
  currentSettings.tx_device = txSel ? txSel.value || "" : "";
  if (colUrl) currentSettings.collector_url = (colUrl.value || "").trim();
  if (histMax) {
    const v = parseInt(histMax.value, 10);
    if (Number.isFinite(v) && v >= 10) currentSettings.tx_history_max = v;
  }
  const preemph = document.getElementById("tx-preemphasis-enabled");
  if (preemph) currentSettings.tx_preemphasis_enabled = !!preemph.checked;
  const deemph = document.getElementById("rx-deemphasis-enabled");
  if (deemph) currentSettings.rx_deemphasis_enabled = !!deemph.checked;

  // Pluto knobs — read MHz, persist Hz. Reject NaN/negative so a
  // half-typed value doesn't poison settings.json.
  const plutoRxFreq = document.getElementById("pluto-rx-freq-mhz");
  if (plutoRxFreq) {
    const mhz = parseFloat(plutoRxFreq.value);
    if (Number.isFinite(mhz) && mhz > 0) {
      currentSettings.pluto_rx_freq_hz = Math.round(mhz * 1e6);
    }
  }
  const plutoTxFreq = document.getElementById("pluto-tx-freq-mhz");
  if (plutoTxFreq) {
    const mhz = parseFloat(plutoTxFreq.value);
    if (Number.isFinite(mhz) && mhz > 0) {
      currentSettings.pluto_tx_freq_hz = Math.round(mhz * 1e6);
    }
  }
  const plutoRxMode = document.getElementById("pluto-rx-gain-mode");
  if (plutoRxMode && plutoRxMode.value) {
    currentSettings.pluto_rx_gain_mode = plutoRxMode.value;
  }
  const plutoRxGain = document.getElementById("pluto-rx-gain-db");
  if (plutoRxGain) {
    const v = parseInt(plutoRxGain.value, 10);
    if (Number.isFinite(v) && v >= -3 && v <= 71) {
      currentSettings.pluto_rx_gain_db = v;
    }
  }
  const plutoTxAtt = document.getElementById("pluto-tx-att-db");
  if (plutoTxAtt) {
    const v = parseFloat(plutoTxAtt.value);
    if (Number.isFinite(v) && v >= 0 && v <= 89.75) {
      currentSettings.pluto_tx_attenuation_db = v;
    }
  }
  // FM deviation : RX preset radio, TX preset radio, TX fine-tune slider.
  // Allowed presets are {2500, 5000} on both directions; offset clamped
  // to [-2000, +2000]. Backend `effective_pluto_tx_deviation_hz` adds
  // them and clamps to a sane band.
  const rxDevRadio = document.querySelector('input[name="pluto-rx-dev"]:checked');
  if (rxDevRadio) {
    const v = parseInt(rxDevRadio.value, 10);
    if (v === 2500 || v === 5000) currentSettings.pluto_rx_deviation_hz = v;
  }
  const txPresetRadio = document.querySelector('input[name="pluto-tx-dev-preset"]:checked');
  if (txPresetRadio) {
    const v = parseInt(txPresetRadio.value, 10);
    if (v === 2500 || v === 5000) currentSettings.pluto_tx_deviation_preset_hz = v;
  }
  const txOffset = document.getElementById("pluto-tx-dev-offset");
  if (txOffset) {
    const v = parseInt(txOffset.value, 10);
    if (Number.isFinite(v) && v >= -2000 && v <= 2000) {
      currentSettings.pluto_tx_deviation_offset_hz = v;
    }
  }
  // CTCSS — sub-audible tone for repeater squelch.
  const ctcssEn = document.getElementById("pluto-tx-ctcss-enabled");
  if (ctcssEn) currentSettings.pluto_tx_ctcss_enabled = !!ctcssEn.checked;
  const ctcssFreq = document.getElementById("pluto-tx-ctcss-freq");
  if (ctcssFreq && ctcssFreq.value) {
    const f = parseFloat(ctcssFreq.value);
    if (Number.isFinite(f) && f >= 67.0 && f <= 254.1) {
      currentSettings.pluto_tx_ctcss_freq_hz = f;
    }
  }
  const statusEl = document.getElementById("settings-status");
  try {
    await invoke("save_settings", { settings: currentSettings });
    if (statusEl) statusEl.textContent = `sauvegardé ${now()}`;
  } catch (err) {
    if (statusEl) statusEl.textContent = `erreur : ${err}`;
  }
}

function setupSettingsTab() {
  const call = document.getElementById("callsign-input");
  const rxSel = document.getElementById("rx-device-select");
  const txSel = document.getElementById("tx-device-select");
  if (call) {
    const onCallsignChange = async () => {
      await persistSettings();
      refreshTxEstimate();
    };
    call.addEventListener("change", onCallsignChange);
    call.addEventListener("blur", onCallsignChange);
  }
  if (rxSel) {
    rxSel.addEventListener("change", () => {
      refreshRxDeviceLabel();
      refreshStartButtonFromRx();
      refreshPlutoPanelVisibility();
      persistSettings();
    });
  }
  // Pluto-specific inputs — persist on every change so the next
  // start_capture picks up new freq / gain without reload.
  ["pluto-rx-freq-mhz", "pluto-tx-freq-mhz", "pluto-rx-gain-mode",
   "pluto-rx-gain-db", "pluto-tx-att-db"].forEach(id => {
    const el = document.getElementById(id);
    if (el) el.addEventListener("change", persistSettings);
  });
  // Deviation radios — persist on selection change.
  document.querySelectorAll('input[name="pluto-rx-dev"]').forEach(r => {
    r.addEventListener("change", persistSettings);
  });
  document.querySelectorAll('input[name="pluto-tx-dev-preset"]').forEach(r => {
    r.addEventListener("change", () => {
      refreshPlutoTxDeviationLabel();
      persistSettings();
    });
  });
  // TX fine-tune slider : live label update on `input`, persist on
  // `change` (when the user releases the slider).
  const txOffset = document.getElementById("pluto-tx-dev-offset");
  if (txOffset) {
    txOffset.addEventListener("input", refreshPlutoTxDeviationLabel);
    txOffset.addEventListener("change", persistSettings);
  }
  // CTCSS dropdown is populated once (39 fixed EIA tones), then
  // its value + the enable checkbox persist on every change.
  populateCtcssDropdown();
  ["pluto-tx-ctcss-enabled", "pluto-tx-ctcss-freq"].forEach(id => {
    const el = document.getElementById(id);
    if (el) el.addEventListener("change", persistSettings);
  });
  if (txSel) {
    txSel.addEventListener("change", () => {
      refreshPlutoPanelVisibility();
      persistSettings();
    });
  }
  const preemph = document.getElementById("tx-preemphasis-enabled");
  if (preemph) preemph.addEventListener("change", persistSettings);
  const deemph = document.getElementById("rx-deemphasis-enabled");
  if (deemph) deemph.addEventListener("change", persistSettings);
  const stopRxBtn = document.getElementById("settings-stop-rx-btn");
  if (stopRxBtn) {
    stopRxBtn.addEventListener("click", async () => {
      stopRxBtn.disabled = true;
      try {
        await stopCapture();
      } catch (err) {
        logEvent("settings_stop_rx_error", { message: String(err) });
      }
      // stopCapture re-calls refreshSettingsRxWarn, so the UI state will
      // be re-synced (banner hidden, RX select re-enabled).
    });
  }
  // PTT widgets : enable/disable + persist.
  const pttEn = document.getElementById("ptt-enabled");
  const pttCfg = document.getElementById("ptt-config");
  if (pttEn) {
    pttEn.addEventListener("change", () => {
      if (pttCfg) pttCfg.hidden = !pttEn.checked;
      persistSettings();
    });
  }
  const pttRefresh = document.getElementById("ptt-port-refresh");
  if (pttRefresh) pttRefresh.addEventListener("click", loadSerialPorts);
  const pttPort = document.getElementById("ptt-port-select");
  if (pttPort) pttPort.addEventListener("change", persistSettings);
  ["ptt-use-rts", "ptt-use-dtr"].forEach(id => {
    const el = document.getElementById(id);
    if (el) el.addEventListener("change", persistSettings);
  });
  document.querySelectorAll('input[name="ptt-rts-pol"], input[name="ptt-dtr-pol"]')
    .forEach(r => r.addEventListener("change", persistSettings));

  // RX force-mode : enable/disable du select et persistance.
  const rxForce = document.getElementById("rx-force-mode");
  const rxForcedSel = document.getElementById("rx-forced-profile");
  if (rxForce) {
    rxForce.addEventListener("change", () => {
      currentSettings.rx_force_mode = rxForce.checked;
      if (rxForcedSel) rxForcedSel.disabled = !rxForce.checked;
      persistSettings();
    });
  }
  if (rxForcedSel) {
    rxForcedSel.addEventListener("change", () => {
      currentSettings.rx_forced_profile = rxForcedSel.value;
      persistSettings();
    });
  }
  const colUrl = document.getElementById("collector-url");
  if (colUrl) {
    colUrl.addEventListener("change", persistSettings);
    colUrl.addEventListener("blur", persistSettings);
  }
  const histMax = document.getElementById("tx-history-max-input");
  if (histMax) {
    histMax.addEventListener("change", persistSettings);
    histMax.addEventListener("blur", persistSettings);
  }
  const expEnabled = document.getElementById("experimental-modes-enabled");
  if (expEnabled) {
    expEnabled.addEventListener("change", () => {
      currentSettings.experimental_modes_enabled = expEnabled.checked;
      applyExperimentalModesToUI();
      persistSettings();
    });
  }
}

async function loadSaveDir() {
  const { invoke } = window.__TAURI__.core;
  try {
    const dir = await invoke("get_save_dir");
    document.getElementById("save-dir-label").textContent = `→ ${dir}`;
    document.getElementById("save-dir-label").title = dir;
  } catch (err) {
    console.error("get_save_dir", err);
  }
}

async function startCapture() {
  const { invoke } = window.__TAURI__.core;
  const select = document.getElementById("rx-device-select");
  const deviceName = select ? select.value : "";
  const status = document.getElementById("status");
  if (!deviceName) {
    status.textContent = "sélectionner une carte RX dans Paramètres";
    status.style.color = "#ef5350";
    return;
  }
  const forced = !!currentSettings.rx_force_mode;
  // If forced, pass the chosen profile; otherwise HIGH (default anchor,
  // auto-detection will refine it).
  const profile = forced ? (currentSettings.rx_forced_profile || "HIGH") : "HIGH";
  try {
    await invoke("start_capture", { deviceName, profile, forced });
    status.textContent = forced
      ? `capture en cours (mode forcé : ${profile})`
      : "capture en cours";
    status.style.color = "#ffb74d";
    document.getElementById("btn-start").disabled = true;
    document.getElementById("btn-stop").disabled = false;
    if (select) select.disabled = true;
    refreshSettingsRxWarn();
    logEvent("start", { device: deviceName, profile, forced });
  } catch (err) {
    status.textContent = `erreur start : ${err}`;
    status.style.color = "#ef5350";
    logEvent("error", { message: String(err) });
  }
}

// Start RX capture if it isn't already running, no TX is occupying the
// audio chain, and a valid RX device is selected. Called at app startup
// and when returning to the RX tab.
async function tryAutoStartCapture() {
  const stopBtn = document.getElementById("btn-stop");
  const startBtn = document.getElementById("btn-start");
  const txStopBtn = document.getElementById("tx-btn-stop");
  if (!stopBtn || !startBtn) return;
  if (!stopBtn.disabled) return;
  if (txStopBtn && !txStopBtn.disabled) return;
  if (startBtn.disabled) return;
  await startCapture();
}

async function stopCapture() {
  const { invoke } = window.__TAURI__.core;
  const status = document.getElementById("status");
  try {
    await invoke("stop_capture");
    status.textContent = "arrêté";
    status.style.color = "#9ccc65";
    document.getElementById("btn-stop").disabled = true;
    const rxSel = document.getElementById("rx-device-select");
    if (rxSel) rxSel.disabled = false;
    refreshStartButtonFromRx();
    refreshSettingsRxWarn();
    await refreshRawRecordingState();
    logEvent("stop", null);
  } catch (err) {
    status.textContent = `erreur stop : ${err}`;
    status.style.color = "#ef5350";
  }
}

let rawRecordingActive = false;

function setRawButtonState(recording) {
  rawRecordingActive = recording;
  const btn = document.getElementById("btn-raw");
  if (recording) {
    btn.classList.add("recording");
    btn.textContent = "⏹ arrêter capture";
  } else {
    btn.classList.remove("recording");
    btn.textContent = "⏺ capture brute";
  }
}

async function refreshRawRecordingState() {
  const { invoke } = window.__TAURI__.core;
  try {
    const active = await invoke("is_raw_recording");
    setRawButtonState(!!active);
  } catch (err) {
    console.error("is_raw_recording", err);
  }
}

async function toggleRawRecording() {
  const { invoke } = window.__TAURI__.core;
  try {
    if (rawRecordingActive) {
      const info = await invoke("stop_raw_recording");
      setRawButtonState(false);
      logEvent("raw_recording_stopped", info);
      maybeOfferCaptureSubmit(info);
    } else {
      const path = await invoke("start_raw_recording");
      setRawButtonState(true);
      logEvent("raw_recording_started", { path });
    }
  } catch (err) {
    logEvent("raw_recording_error", { message: String(err) });
  }
}

// ─────────────────────────────────────────── Submit capture (Phase D)
// If the user has set a collector URL in Settings, we show a panel right
// after the end of a raw capture to offer submission. Otherwise, nothing -
// we only submit on explicit request.
let pendingCapture = null;

function maybeOfferCaptureSubmit(captureInfo) {
  const url = (currentSettings.collector_url || "").trim();
  const panel = document.getElementById("capture-submit-prompt");
  if (!panel) return;
  if (!url) {
    panel.hidden = true;
    pendingCapture = null;
    return;
  }
  pendingCapture = captureInfo;
  panel.hidden = false;
  panel.classList.remove("busy", "success", "error");
  const meta = document.getElementById("csp-meta");
  if (meta) {
    const sizeMb = (captureInfo.samples * 4 / (1024 * 1024)).toFixed(1);
    meta.textContent = `${captureInfo.duration_sec.toFixed(1)} s · ~${sizeMb} MB · ${captureInfo.path}`;
  }
  const status = document.getElementById("csp-status");
  if (status) status.textContent = `prêt à soumettre vers ${url}`;
  const submit = document.getElementById("csp-submit");
  const dismiss = document.getElementById("csp-dismiss");
  if (submit) submit.disabled = false;
  if (dismiss) dismiss.disabled = false;
  const notes = document.getElementById("csp-notes");
  if (notes) notes.value = "";
}

async function submitPendingCapture() {
  if (!pendingCapture) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  const panel = document.getElementById("capture-submit-prompt");
  const status = document.getElementById("csp-status");
  const submit = document.getElementById("csp-submit");
  const dismiss = document.getElementById("csp-dismiss");
  const notesEl = document.getElementById("csp-notes");
  const notes = (notesEl && notesEl.value || "").trim() || null;
  if (panel) panel.classList.add("busy");
  if (submit) submit.disabled = true;
  if (dismiss) dismiss.disabled = true;
  if (status) status.textContent = "envoi en cours…";
  try {
    const result = await invoke("submit_capture", {
      args: {
        wav_path: pendingCapture.path,
        callsign: currentSettings.callsign || "",
        collector_url: (currentSettings.collector_url || "").trim(),
        profile: currentProfile || null,
        notes,
        event_log_json: JSON.stringify(eventLogBuffer),
      },
    });
    panel.classList.remove("busy");
    panel.classList.add("success");
    const base = (currentSettings.collector_url || "").replace(/\/+$/, "");
    const fullUrl = base + (result.url || "");
    if (status) {
      status.innerHTML = `envoyé : <a href="${escapeHtml(fullUrl)}" target="_blank">${escapeHtml(result.folder)}</a> ` +
        `(${(result.bytes_uploaded / (1024 * 1024)).toFixed(1)} MB)`;
    }
    if (dismiss) {
      dismiss.disabled = false;
      dismiss.textContent = "Fermer";
    }
    logEvent("capture_submit_ok", { folder: result.folder, bytes: result.bytes_uploaded });
    pendingCapture = null;
  } catch (err) {
    panel.classList.remove("busy");
    panel.classList.add("error");
    if (status) status.textContent = `erreur : ${err}`;
    if (submit) submit.disabled = false;
    if (dismiss) dismiss.disabled = false;
    logEvent("capture_submit_error", { message: String(err) });
  }
}

function dismissCapturePrompt() {
  const panel = document.getElementById("capture-submit-prompt");
  if (panel) {
    panel.hidden = true;
    panel.classList.remove("busy", "success", "error");
  }
  const dismiss = document.getElementById("csp-dismiss");
  if (dismiss) dismiss.textContent = "Ignorer";
  pendingCapture = null;
}

// ─────────────────────────────────────────── Overlays tab
// Single source of truth = `currentSettings.overlays` + `currentSettings.active_overlay`.
// Slot 0 is the immutable "Aucun" entry. Slots 1..=4 are user-editable
// templates. On every edit we update currentSettings, persist, refresh
// the slot label, and trigger a TX recompress so the preview matches
// what will be transmitted.

function getActiveOverlayPayload() {
  ensureOverlaySlots();
  const idx = currentSettings.active_overlay | 0;
  if (idx <= 0 || idx >= currentSettings.overlays.length) return null;
  const slot = currentSettings.overlays[idx];
  if (!slot) return null;
  const text = slot.text && slot.text.content ? slot.text : null;
  const logo = slot.logo && slot.logo.filename ? slot.logo : null;
  if (!text && !logo) return null;
  return { name: slot.name || "", text, logo };
}

function setSlotLabel(idx, name) {
  const span = document.querySelector(`.ov-slot-label[data-slot="${idx}"]`);
  if (!span) return;
  const empty = !name || /^Overlay \d+$/.test(name);
  span.textContent = name && name.trim() ? name : `Overlay ${idx}`;
  span.classList.toggle("ov-empty", empty);
}

function refreshSlotLabels() {
  for (let i = 1; i <= 4; i++) {
    const slot = currentSettings.overlays[i] || {};
    setSlotLabel(i, slot.name || "");
  }
}

function getEditingSlot() {
  const idx = currentSettings.active_overlay | 0;
  if (idx <= 0) return null;
  return currentSettings.overlays[idx] || null;
}

function applyOverlayEditorFromState() {
  const editor = document.getElementById("ov-editor");
  const slot = getEditingSlot();
  if (!editor) return;
  if (!slot) {
    editor.hidden = true;
    return;
  }
  editor.hidden = false;
  document.getElementById("ov-name").value = slot.name || "";
  // Text element.
  const t = slot.text || {};
  document.getElementById("ov-text-enable").checked = !!slot.text;
  document.getElementById("ov-text-content").value = t.content || "";
  document.getElementById("ov-text-anchor").value = t.anchor || "bottom_right";
  document.getElementById("ov-text-mx").value = (t.margin_x_pct ?? 2);
  document.getElementById("ov-text-my").value = (t.margin_y_pct ?? 2);
  document.getElementById("ov-text-h").value = (t.height_pct ?? 6);
  document.getElementById("ov-text-color").value = t.color || "#ffffff";
  document.getElementById("ov-text-halo").checked = t.halo !== false;
  // Logo element.
  const l = slot.logo || {};
  document.getElementById("ov-logo-enable").checked = !!slot.logo;
  document.getElementById("ov-logo-anchor").value = l.anchor || "top_left";
  document.getElementById("ov-logo-mx").value = (l.margin_x_pct ?? 2);
  document.getElementById("ov-logo-my").value = (l.margin_y_pct ?? 2);
  document.getElementById("ov-logo-size").value = (l.size_pct ?? 12);
  refreshLogoPreview(l.filename || "");
}

async function refreshLogoPreview(filename) {
  const nameEl = document.getElementById("ov-logo-name");
  const imgEl = document.getElementById("ov-logo-preview");
  if (!nameEl || !imgEl) return;
  if (!filename) {
    nameEl.textContent = "—";
    imgEl.hidden = true;
    imgEl.src = "";
    return;
  }
  nameEl.textContent = filename;
  if (window.__TAURI__ && window.__TAURI__.core) {
    const { invoke, convertFileSrc } = window.__TAURI__.core;
    try {
      const dir = await invoke("overlays_logos_dir");
      const sep = dir.includes("\\") ? "\\" : "/";
      const url = `${convertFileSrc(`${dir}${sep}${filename}`)}?v=${Date.now()}`;
      imgEl.src = url;
      imgEl.hidden = false;
    } catch (err) {
      console.error("overlays_logos_dir", err);
      imgEl.hidden = true;
    }
  }
}

function readOverlayEditorIntoState() {
  const slot = getEditingSlot();
  if (!slot) return;
  slot.name = (document.getElementById("ov-name").value || "").trim();
  // Text.
  const tEnabled = document.getElementById("ov-text-enable").checked;
  if (tEnabled) {
    slot.text = {
      content: document.getElementById("ov-text-content").value || "",
      anchor: document.getElementById("ov-text-anchor").value,
      margin_x_pct: numOr(document.getElementById("ov-text-mx").value, 2),
      margin_y_pct: numOr(document.getElementById("ov-text-my").value, 2),
      height_pct: numOr(document.getElementById("ov-text-h").value, 6),
      color: document.getElementById("ov-text-color").value || "#ffffff",
      halo: document.getElementById("ov-text-halo").checked,
    };
  } else {
    slot.text = null;
  }
  // Logo. The filename is owned by the import flow, so we preserve it
  // even when the user toggles "Logo" off + on; we only nullify on off.
  const lEnabled = document.getElementById("ov-logo-enable").checked;
  if (lEnabled) {
    slot.logo = {
      filename: (slot.logo && slot.logo.filename) || "",
      anchor: document.getElementById("ov-logo-anchor").value,
      margin_x_pct: numOr(document.getElementById("ov-logo-mx").value, 2),
      margin_y_pct: numOr(document.getElementById("ov-logo-my").value, 2),
      size_pct: numOr(document.getElementById("ov-logo-size").value, 12),
    };
  } else {
    slot.logo = null;
  }
}

function numOr(v, fallback) {
  const n = parseFloat(v);
  return Number.isFinite(n) ? n : fallback;
}

let _overlayCommitTimer = null;
function commitOverlayChange() {
  readOverlayEditorIntoState();
  refreshSlotLabels();
  if (_overlayCommitTimer) clearTimeout(_overlayCommitTimer);
  _overlayCommitTimer = setTimeout(() => {
    _overlayCommitTimer = null;
    persistSettings();
    if (txState.sourceFile && !txState.fileMode) scheduleTxCompress(50);
  }, 250);
}

async function pickOverlayLogo(file) {
  if (!file) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  const buf = await file.arrayBuffer();
  try {
    const filename = await invoke("overlays_import_logo", {
      bytes: Array.from(new Uint8Array(buf)),
      originalName: file.name || "logo.png",
    });
    const slot = getEditingSlot();
    if (!slot) return;
    if (!slot.logo) {
      slot.logo = { filename, anchor: "top_left", margin_x_pct: 2, margin_y_pct: 2, size_pct: 12 };
    } else {
      slot.logo.filename = filename;
    }
    document.getElementById("ov-logo-enable").checked = true;
    applyOverlayEditorFromState();
    commitOverlayChange();
  } catch (err) {
    console.error("overlays_import_logo", err);
    alert(`Import logo échoué : ${err}`);
  }
}

function setupOverlaysTab() {
  ensureOverlaySlots();
  // Slot selection (radio bar).
  const radios = document.querySelectorAll('input[name="active-overlay"]');
  radios.forEach(r => {
    r.addEventListener("change", () => {
      currentSettings.active_overlay = parseInt(r.value, 10) | 0;
      applyOverlayEditorFromState();
      persistSettings();
      if (txState.sourceFile && !txState.fileMode) scheduleTxCompress(50);
    });
  });
  // All editable inputs flow through commitOverlayChange.
  const inputIds = [
    "ov-name",
    "ov-text-enable", "ov-text-content", "ov-text-anchor",
    "ov-text-mx", "ov-text-my", "ov-text-h", "ov-text-color", "ov-text-halo",
    "ov-logo-enable", "ov-logo-anchor",
    "ov-logo-mx", "ov-logo-my", "ov-logo-size",
  ];
  for (const id of inputIds) {
    const el = document.getElementById(id);
    if (!el) continue;
    el.addEventListener("input", commitOverlayChange);
    el.addEventListener("change", commitOverlayChange);
  }
  // Logo file picker.
  const pick = document.getElementById("ov-logo-pick");
  const fileInput = document.getElementById("ov-logo-input");
  if (pick && fileInput) {
    pick.addEventListener("click", () => fileInput.click());
    fileInput.addEventListener("change", () => {
      const f = fileInput.files && fileInput.files[0];
      if (f) pickOverlayLogo(f);
      fileInput.value = "";
    });
  }
}

function applyOverlaysToUI() {
  ensureOverlaySlots();
  refreshSlotLabels();
  const idx = currentSettings.active_overlay | 0;
  const radio = document.querySelector(`input[name="active-overlay"][value="${idx}"]`);
  if (radio) radio.checked = true;
  applyOverlayEditorFromState();
}

function setupCaptureSubmitPanel() {
  const submit = document.getElementById("csp-submit");
  const dismiss = document.getElementById("csp-dismiss");
  if (submit) submit.addEventListener("click", submitPendingCapture);
  if (dismiss) dismiss.addEventListener("click", dismissCapturePrompt);
}

function updateLevel(rms, peak, _totalSamples) {
  const fill = document.getElementById("level-fill");
  const text = document.getElementById("level-text");
  const db = rms > 1e-6 ? 20 * Math.log10(rms) : -120;
  const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  fill.style.width = `${pct}%`;
  const dbStr = db.toFixed(1).padStart(6, " ");
  const peakStr = peak.toFixed(2).padStart(4, " ");
  text.textContent = `${dbStr} dB · peak ${peakStr}`;
}

// #HB9TOB: how long the OVD chip stays red after the last detection. The
// chip clears itself after this delay if no further batch is flagged
// overdrive. See OVERDRIVE_* on the Rust side for the detection threshold.
const OVD_STICKY_MS = 5000;

let lastOverdriveMs = 0;
let lastCrestDb = NaN;

function refreshOverdriveChip() {
  const chip = document.getElementById("ovd-chip");
  if (!chip) return;
  const active = lastOverdriveMs > 0 && (Date.now() - lastOverdriveMs) < OVD_STICKY_MS;
  chip.classList.toggle("ovd-on", active);
  chip.classList.toggle("ovd-off", !active);
  if (Number.isFinite(lastCrestDb)) {
    chip.title = `Overdrive TX — crest ${lastCrestDb.toFixed(1)} dB (seuil 8.5 dB)`;
  }
}

function noteAudioOverdrive(overdrive, crestDb) {
  if (Number.isFinite(crestDb)) lastCrestDb = crestDb;
  if (overdrive) lastOverdriveMs = Date.now();
  refreshOverdriveChip();
}

function noteProfileFromHeader(_profileStr) {}

function updateV2State(state) {
  const chip = document.getElementById("v2-state-chip");
  chip.className = `state-chip state-${state}`;
  chip.textContent = state.replace(/_/g, " ");
  if (state === "idle") {
    document.getElementById("v2-marker-info").textContent = "—";
    resetRxVisuals();
    noteProfileFromHeader(null);
  }
}

function updateV2Marker(payload) {
  const info = document.getElementById("v2-marker-info");
  const kind = payload.is_meta ? "meta" : "data";
  const seg = String(payload.seg_id).padStart(2, " ");
  const esi = String(payload.base_esi).padStart(4, " ");
  info.textContent = `seg=${seg} esi=${esi} ${kind}`;
}

// ─────────────────────────────── Per-block progress + constellation state
let lastProgress = {
  bitmap: null,
  expected: 0,
  converged: 0,
  sigma2: null,
};
let lastConstellation = [];
let lastPilotPhases = [];

function resetRxVisuals() {
  lastProgress = { bitmap: null, expected: 0, converged: 0, sigma2: null };
  lastConstellation = [];
  lastPilotPhases = [];
  const text = document.getElementById("v2-progress-text");
  if (text) text.textContent = "—";
  hideFountainStatus();
  drawProgressBlocks();
  drawConstellation();
  drawPilotPhase();
}

function hideFountainStatus() {
  fountainState = { sessionId: null, received: 0, needed: 0, decoded: false, capReached: false };
  const el = document.getElementById("rx-fountain-status");
  if (el) el.hidden = true;
}

let fountainState = { sessionId: null, received: 0, needed: 0, decoded: false, capReached: false };

function updateFountainStatus(partial) {
  // Merge : null/undefined fields in `partial` leave the previous value in
  // place. This matters for session_decoded (which may not re-send
  // received / needed) and peek-re-announce paths.
  const next = { ...fountainState };
  for (const [k, v] of Object.entries(partial)) {
    if (v !== null && v !== undefined) next[k] = v;
  }
  fountainState = next;
  const el = document.getElementById("rx-fountain-status");
  const counter = document.getElementById("rx-fountain-counter");
  const pct = document.getElementById("rx-fountain-pct");
  const sess = document.getElementById("rx-fountain-session");
  if (!el || !counter || !pct || !sess) return;
  el.hidden = false;
  const k = next.needed || 0;
  const r = next.received || 0;
  // Don't cap "received" at K - the user is allowed to see they have
  // already swallowed more blocks than the strict minimum (repair
  // included). "Missing" cannot go negative: it's max(0, K - R).
  const missing = Math.max(0, k - r);
  const missingTail = next.decoded
    ? ""
    : missing > 0
    ? ` · manque ${missing}`
    : ` · manque 0 (décodable)`;
  counter.textContent = `${r} / ${k} blocs${missingTail}`;
  const pctVal = k > 0 ? Math.min(100, Math.round((r * 100) / k)) : 0;
  pct.textContent = next.decoded
    ? "décodé ✓"
    : next.capReached
    ? `${pctVal} % (canal saturé)`
    : `${pctVal} %`;
  if (next.sessionId != null) {
    sess.textContent = `session ${next.sessionId.toString(16).padStart(8, "0")}`;
  }
  el.dataset.decoded = next.decoded ? "true" : "false";
}

function updateV2Progress(payload) {
  // Bitmap may arrive as an Array (JSON) — each byte = 8 consecutive ESIs,
  // LSB-first. We store it as Uint8Array for fast bit tests in the render.
  const bm = payload.converged_bitmap;
  const bitmap = bm
    ? new Uint8Array(bm)
    : new Uint8Array(Math.ceil((payload.blocks_expected || 0) / 8));
  lastProgress = {
    bitmap,
    expected: payload.blocks_expected || 0,
    converged: payload.blocks_converged || 0,
    sigma2: Number.isFinite(payload.sigma2) ? payload.sigma2 : null,
  };
  lastConstellation = Array.isArray(payload.constellation_sample)
    ? payload.constellation_sample
    : [];
  lastPilotPhases = Array.isArray(payload.pilot_phase_segments)
    ? payload.pilot_phase_segments
    : [];

  const sigmaStr = lastProgress.sigma2 != null
    ? lastProgress.sigma2.toFixed(3).padStart(6, " ")
    : "     ?";
  const mini = document.getElementById("v2-progress-text");
  if (mini) {
    const c = String(lastProgress.converged).padStart(3, " ");
    const e = String(lastProgress.expected).padStart(3, " ");
    mini.textContent = `${c}/${e} σ²=${sigmaStr}`;
  }
  drawProgressBlocks();
  drawConstellation();
  drawPilotPhase();
}

function redrawAll() {
  drawProgressBlocks();
  drawConstellation();
  drawPilotPhase();
}

function drawProgressBlocks() {
  const canvas = document.getElementById("progress-blocks");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  // Match canvas pixel size to CSS size for crisp lines.
  const rect = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  if (
    canvas.width !== Math.round(rect.width * dpr) ||
    canvas.height !== Math.round(rect.height * dpr)
  ) {
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
  }
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  const { bitmap, expected, converged } = lastProgress;
  if (!expected || expected <= 0) {
    ctx.fillStyle = "#3a1a1a";
    ctx.fillRect(0, 0, w, h);
    return;
  }
  // "Fountain fill" strategy: the RaptorQ code doesn't need to recover
  // the missing ESIs exactly - K total blocks is enough. So we display
  // the actual bitmap (ESI positions effectively received), then we
  // "plug the holes" as soon as `converged` exceeds the number of bits
  // set in [0..expected): ESIs > expected (coming from More or repair)
  // are not lost, they repaint the first red hole.
  const bw = w / expected;
  const slotConverged = new Array(expected).fill(false);
  let filled = 0;
  if (bitmap) {
    for (let i = 0; i < expected; i++) {
      const byte = bitmap[i >> 3] || 0;
      if (((byte >> (i & 7)) & 1) !== 0) {
        slotConverged[i] = true;
        filled++;
      }
    }
  }
  // Surplus = blocks received beyond what the local bitmap can show.
  // Fills the holes from left to right.
  let surplus = Math.max(0, (converged || 0) - filled);
  if (surplus > 0) {
    for (let i = 0; i < expected && surplus > 0; i++) {
      if (!slotConverged[i]) {
        slotConverged[i] = true;
        surplus--;
      }
    }
  }
  for (let i = 0; i < expected; i++) {
    ctx.fillStyle = slotConverged[i] ? "#9ccc65" : "#c62828";
    ctx.fillRect(Math.floor(i * bw), 0, Math.max(1, Math.ceil(bw) - 1), h);
  }
}

function drawConstellation() {
  const canvas = document.getElementById("constellation-canvas");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  // Subtle grid + axes.
  ctx.strokeStyle = "#2a2a2a";
  ctx.lineWidth = 1;
  for (let i = 1; i < 4; i++) {
    const x = (i * w) / 4;
    const y = (i * h) / 4;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, h);
    ctx.moveTo(0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
  }
  ctx.strokeStyle = "#4a4a4a";
  ctx.beginPath();
  ctx.moveTo(w / 2, 0);
  ctx.lineTo(w / 2, h);
  ctx.moveTo(0, h / 2);
  ctx.lineTo(w, h / 2);
  ctx.stroke();

  const pts = lastConstellation;
  if (!pts.length) return;
  // Scale: constellation points are unit-magnitude-ish post-correction
  // (±1 for QPSK, up to ~1.5 for 16-APSK outer ring). Map ±1.7 to canvas.
  const scale = (Math.min(w, h) / 2) / 1.7;
  ctx.fillStyle = "rgba(129, 212, 250, 0.85)";
  for (const p of pts) {
    const x = w / 2 + p[0] * scale;
    const y = h / 2 - p[1] * scale;
    ctx.beginPath();
    ctx.arc(x, y, 2.2, 0, 2 * Math.PI);
    ctx.fill();
  }
}

function drawPilotPhase() {
  const canvas = document.getElementById("pilot-phase-canvas");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  // Match canvas pixel size to CSS for crisp lines.
  const rect = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  if (
    canvas.width !== Math.round(rect.width * dpr) ||
    canvas.height !== Math.round(rect.height * dpr)
  ) {
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
  }
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);

  // Reserve a 56-px gutter on the left for the Y-axis labels so the
  // mrad scale stays readable even on a narrow window.
  const gutter = 56 * dpr;
  const plotX0 = gutter;
  const plotW = w - gutter;

  const segments = lastPilotPhases;
  const total = segments.reduce((acc, s) => acc + s.length, 0);
  if (!segments.length || total < 2) {
    ctx.fillStyle = "#1a1a1a";
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = "#888";
    ctx.font = `bold ${13 * dpr}px monospace`;
    ctx.fillText("phase pilote — en attente", 8 * dpr, 18 * dpr);
    return;
  }

  // Re-anchor each segment to its first sample so the plot shows
  // INTRA-SEGMENT drift only (between-segment jumps come from pilot
  // interp restart, which would dominate the y-range).
  const anchored = segments.map((seg) => {
    const a0 = seg[0] || 0;
    return seg.map((p) => p - a0);
  });

  let ymin = Infinity;
  let ymax = -Infinity;
  for (const seg of anchored) {
    for (const v of seg) {
      if (v < ymin) ymin = v;
      if (v > ymax) ymax = v;
    }
  }
  if (!isFinite(ymin) || !isFinite(ymax)) return;
  const span = ymax - ymin;
  // Floor the range so a near-flat trace doesn't get blown up to ±1 mrad
  // and look noisy for nothing. 50 mrad minimum spread.
  const minHalf = 0.05;
  if (span < 2 * minHalf) {
    const center = (ymax + ymin) / 2;
    ymin = center - minHalf;
    ymax = center + minHalf;
  } else {
    const pad = span * 0.15;
    ymin -= pad;
    ymax += pad;
  }
  const yRange = ymax - ymin;

  // Background panel for the plot area.
  ctx.fillStyle = "rgba(0,0,0,0.0)"; // canvas already dark
  ctx.fillRect(plotX0, 0, plotW, h);

  // Y-axis ticks : 5 levels from ymin to ymax with mrad labels.
  ctx.strokeStyle = "#2a2a2a";
  ctx.fillStyle = "#aaa";
  ctx.font = `${11 * dpr}px monospace`;
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  ctx.lineWidth = 1 * dpr;
  const nTicks = 5;
  for (let i = 0; i <= nTicks; i++) {
    const t = i / nTicks;
    const y = h - t * h;
    const valRad = ymin + t * yRange;
    const valMrad = valRad * 1000;
    ctx.beginPath();
    ctx.moveTo(plotX0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
    const lbl = Math.abs(valMrad) < 10
      ? valMrad.toFixed(1)
      : valMrad.toFixed(0);
    ctx.fillText(`${lbl}`, plotX0 - 4 * dpr, y);
  }
  // Y-axis label
  ctx.save();
  ctx.translate(14 * dpr, h / 2);
  ctx.rotate(-Math.PI / 2);
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillStyle = "#888";
  ctx.font = `${11 * dpr}px monospace`;
  ctx.fillText("phase (mrad)", 0, 0);
  ctx.restore();

  // Zero baseline highlighted
  const yZero = h - ((0 - ymin) / yRange) * h;
  if (yZero >= 0 && yZero <= h) {
    ctx.strokeStyle = "#5a5a5a";
    ctx.lineWidth = 1.5 * dpr;
    ctx.beginPath();
    ctx.moveTo(plotX0, yZero);
    ctx.lineTo(w, yZero);
    ctx.stroke();
  }

  // Walk each segment as a polyline. Alternate stroke colour per segment
  // so the user can count them and see where pilot interp restarts.
  const colours = ["rgba(129, 212, 250, 0.95)", "rgba(255, 183, 77, 0.95)"];
  let xCursor = 0;
  const pxPerSample = total > 1 ? plotW / (total - 1) : 0;
  for (let s = 0; s < anchored.length; s++) {
    const seg = anchored[s];
    if (seg.length === 0) continue;
    ctx.strokeStyle = colours[s % colours.length];
    ctx.lineWidth = 2 * dpr;
    ctx.beginPath();
    for (let i = 0; i < seg.length; i++) {
      const x = plotX0 + (xCursor + i) * pxPerSample;
      const y = h - ((seg[i] - ymin) / yRange) * h;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
    xCursor += seg.length;
    if (s < anchored.length - 1) {
      const xb = plotX0 + xCursor * pxPerSample;
      ctx.strokeStyle = "#444";
      ctx.lineWidth = 1 * dpr;
      ctx.setLineDash([4 * dpr, 3 * dpr]);
      ctx.beginPath();
      ctx.moveTo(xb, 0);
      ctx.lineTo(xb, h);
      ctx.stroke();
      ctx.setLineDash([]);
    }
  }

  // Header overlay : range, segments count, σ² (if available), implied SNR.
  const rangeMrad = (yRange * 1000).toFixed(0);
  const sigma2 = lastProgress.sigma2;
  let header = `±${(rangeMrad / 2).toFixed(0)} mrad · ${segments.length} seg`;
  if (sigma2 != null && sigma2 > 0) {
    const snrDb = (-10 * Math.log10(sigma2)).toFixed(1);
    header += ` · σ²=${sigma2.toFixed(3)} (${snrDb} dB)`;
  }
  ctx.fillStyle = "rgba(0,0,0,0.55)";
  ctx.fillRect(plotX0, 0, plotW, 22 * dpr);
  ctx.fillStyle = "#e0e0e0";
  ctx.font = `bold ${12 * dpr}px monospace`;
  ctx.textAlign = "left";
  ctx.textBaseline = "top";
  ctx.fillText(header, plotX0 + 6 * dpr, 4 * dpr);
}

function wireEvents() {
  const { listen } = window.__TAURI__.event;
  const names = [
    "preamble",
    "header",
    "app_header",
    "envelope",
    "progress",
    "file_complete",
    "session_end",
    "error",
  ];
  for (const name of names) {
    listen(name, (event) => {
      logEvent(name, event.payload);
      if (name === "file_complete") {
        showCurrentFile(event.payload);
        // Reveal-in-folder only for non-image files. For images we
        // already have the preview in the RX tab + the history, opening
        // the folder would be intrusive (focus stealing).
        if (!isImageMime(event.payload.mime_type)) {
          revealReceivedFile(event.payload.saved_path);
        }
      }
      if (name === "header" && event.payload && event.payload.profile) {
        noteProfileFromHeader(event.payload.profile);
      }
    });
  }
  listen("audio_level", (event) => {
    const p = event.payload;
    updateLevel(p.rms, p.peak, p.total_samples);
    noteAudioOverdrive(!!p.overdrive, p.crest_db);
  });

  listen("tx_plan", (ev) => {
    logEvent("tx_plan", ev.payload);
  });
  listen("tx_progress", (ev) => {
    onTxProgress(ev.payload);
  });
  listen("tx_complete", (ev) => {
    onTxComplete(ev.payload);
  });
  listen("tx_error", (ev) => {
    onTxError(ev.payload);
  });
  listen("ptt_status", (ev) => {
    renderPttStatus(ev.payload);
    logEvent("ptt_status", ev.payload);
  });
  listen("v2_progress", (event) => {
    updateV2Progress(event.payload);
    // Log the progress event WITHOUT the bitmap/constellation arrays,
    // which would clutter the Info tab with tens of KB per event.
    const p = event.payload || {};
    logEvent("v2_progress", {
      blocks_converged: p.blocks_converged,
      blocks_total: p.blocks_total,
      blocks_expected: p.blocks_expected,
      sigma2: p.sigma2,
    });
  });
  listen("session_armed", (event) => {
    const p = event.payload || {};
    upsertSession({
      session_id: p.session_id,
      k_symbols: p.k,
      t_bytes: p.t,
      file_size: p.file_size,
      mime_type: p.mime_type,
      profile: p.profile,
      received: 0,
      decoded: false,
      cap_reached: false,
      created_at: Math.floor(Date.now() / 1000),
    });
    updateFountainStatus({
      sessionId: p.session_id,
      received: 0,
      needed: p.k,
      decoded: false,
      capReached: false,
    });
    logEvent("session_armed", p);
  });
  listen("session_progress", (event) => {
    const p = event.payload || {};
    upsertSession({
      session_id: p.session_id,
      received: p.received,
      k_symbols: p.needed,
      decoded: !!p.decoded,
      cap_reached: !!p.cap_reached,
    });
    updateFountainStatus({
      sessionId: p.session_id,
      received: p.received,
      needed: p.needed,
      decoded: !!p.decoded,
      capReached: !!p.cap_reached,
    });
  });
  listen("session_decoded", (event) => {
    const p = event.payload || {};
    upsertSession({
      session_id: p.session_id,
      decoded: true,
      filename: p.filename,
      callsign: p.callsign,
    });
    updateFountainStatus({
      sessionId: p.session_id,
      received: null,
      needed: null,
      decoded: true,
      capReached: false,
    });
    logEvent("session_decoded", p);
    // Refresh the RX column of the History tab. Lightweight: one
    // read_dir + parsing each session's meta.json.
    refreshHistory().catch(() => {});
  });
  listen("tx_archived", () => {
    // Emitted by tx_worker::archive_payload at the start of every transmission.
    refreshHistory().catch(() => {});
  });
}

// ────────────────────────────────────────────────────────────── TX tab (GUI)
// The backend wiring (AVIF encoding, TX launch, audio rendering) comes
// later. Here we only handle: file loading (picker + DnD), target
// dimensions with aspect ratio respected, state of the controls.
const txState = {
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

// Promise chain to serialize AVIF compressions. Without it, dropping an
// image while a compression is running launches a 2nd ravif speed-1
// encoder in parallel - enough to saturate RAM and freeze KDE on large
// images.
let _compressChain = Promise.resolve();

// Transport limits. Image: <= 100 kB + <= 5 min (warn > 2 min). Non-image
// file: <= 10 min (warn > 5 min), no extra size limit - duration is the
// real NBFM constraint.
const TX_HARD_BYTES = 100 * 1024;
const TX_HARD_SECONDS = 5 * 60;
const TX_WARN_SECONDS = 2 * 60;
const TX_FILE_HARD_SECONDS = 10 * 60;
const TX_FILE_WARN_SECONDS = 5 * 60;

function fmtSeconds(s) {
  if (!Number.isFinite(s)) return "—";
  const m = Math.floor(s / 60);
  const r = Math.round(s - m * 60);
  return `${m}:${String(r).padStart(2, "0")}`;
}

function refreshTxExperimentalWarn() {
  const warn = document.getElementById("tx-experimental-warn");
  if (!warn) return;
  // Source of truth: ProfileDescriptor.experimental from modem-core (cf.
  // V3Modem in modem-core/src/v3_modem.rs). Adding/removing an experimental
  // profile in core automatically re-flags the warning here.
  const desc = modemProfiles.find((p) => p.name === txState.mode);
  warn.hidden = !(desc && desc.experimental);
}

function refreshTxButtons() {
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
    btnTx.textContent = "TX en cours…";
    btnTx.title = "émission en cours";
  } else if (tooBig) {
    btnTx.textContent = `TX ✖ image > 100 ko`;
    btnTx.title = `${(bytes / 1024).toFixed(1)} Kio dépasse la limite 100 Kio (images)`;
  } else if (tooLong) {
    const limMin = isFile ? 10 : 5;
    btnTx.textContent = `TX ✖ > ${limMin} min`;
    btnTx.title = `durée estimée ${fmtSeconds(dur)} dépasse la limite ${limMin} min`;
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
}

function txFormatBytes(n) {
  if (n == null) return "—";
  if (n < 1024) return `${n} o`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} Kio`;
  return `${(n / 1024 / 1024).toFixed(2)} Mio`;
}

// TX button tooltip: duration, N emitted, K required, K threshold.
function txButtonTitle(est, dur, longTx) {
  if (!est) return "";
  const base = longTx ? `transmission longue (> 2 min) — durée ` : `durée `;
  const k = est.k_source;
  const n = est.n_initial ?? est.total_blocks;
  const parts = [`${base}${dur}, ${n} blocs émis`];
  if (k != null && k !== n) {
    parts.push(`K=${k} nécessaires au décodage`);
  }
  if (est.duration_s_k != null) {
    parts.push(`seuil ${fmtSeconds(est.duration_s_k)} si aucune perte`);
  }
  return parts.join(" · ");
}

// More button tooltip: additional blocks, expected duration.
function moreButtonTitle() {
  const est = txState.estimate;
  const count = computeMoreCount();
  if (!est || !est.seconds_per_cw) {
    return `émettre +${count} blocs RaptorQ`;
  }
  const dur = est.seconds_per_cw * count;
  return `+${count} blocs · ~${fmtSeconds(dur)}`;
}

function txFitInto(w, h, maxW, maxH) {
  const s = Math.min(maxW / w, maxH / h, 1);
  return { w: Math.max(1, Math.round(w * s)), h: Math.max(1, Math.round(h * s)) };
}

function txTargetDims() {
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

function refreshTxPreview() {
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
      cmpSize.textContent = "compression…";
      cmpSize.classList.remove("tx-stale");
    } else if (txState.compressedBytes != null) {
      const ratio = txState.sourceSize > 0
        ? ` (${(txState.compressedBytes / txState.sourceSize * 100).toFixed(1)}%)`
        : "";
      const staleTag = txState.compressDirty ? " · obsolète" : "";
      cmpSize.textContent = `${txFormatBytes(txState.compressedBytes)}${ratio}${staleTag}`;
      cmpSize.classList.toggle("tx-stale", txState.compressDirty);
    } else {
      cmpSize.textContent = "—";
      cmpSize.classList.remove("tx-stale");
    }
  }
}

function scheduleTxCompress(delayMs = 300) {
  if (txState.compressTimer) clearTimeout(txState.compressTimer);
  txState.compressTimer = setTimeout(() => {
    txState.compressTimer = null;
    runTxCompress();
  }, delayMs);
}

function getTxFilename() {
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

async function refreshTxEstimate() {
  txState.estimate = null;
  if (!txState.compressedBytes) {
    refreshTxButtons();
    refreshTxPreview();
    return;
  }
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  try {
    const est = await invoke("tx_estimate", {
      payloadBytes: txState.compressedBytes,
      mode: txState.mode,
      callsign: currentSettings.callsign || "HB9XXX",
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

function runTxCompress() {
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

async function _runTxCompressImpl() {
  if (!txState.sourceFile) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke, convertFileSrc } = window.__TAURI__.core;
  const seq = ++txState.compressSeq;
  txState.compressing = true;
  const previewEl = document.getElementById("tx-preview");
  if (previewEl) previewEl.classList.add("compressing");
  refreshTxPreview();
  refreshTxButtons();
  // Force the browser to paint the loader before launching invoke().
  await new Promise((r) =>
    requestAnimationFrame(() => requestAnimationFrame(r)),
  );
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
      // a real decode/re-encode — so force passthrough off whenever an
      // overlay is present, even on AVIF sources.
      const ov = getActiveOverlayPayload();
      const result = await invoke("compress_image", {
        opts: {
          target_w: dims.w,
          target_h: dims.h,
          quality: txState.quality,
          speed: txState.speed,
          passthrough: !!txState.avifPassthrough && !ov,
          overlay: ov,
        },
      });
      if (seq !== txState.compressSeq) return; // stale
      txState.compressedBytes = result.byte_len;
      const url = `${convertFileSrc(result.preview_path)}?v=${Date.now()}`;
      txState.compressedUrl = url;
      txState.compressDirty = false;
      const previewImg = document.getElementById("tx-preview-img");
      if (previewImg) previewImg.src = url;
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
      const el = document.getElementById("tx-preview");
      if (el) el.classList.remove("compressing");
      refreshTxPreview();
      refreshTxButtons();
    }
  }
}

// Image-extension detection - if false, switch to the file/zstd flow.
const IMAGE_EXTS = new Set(["png", "jpg", "jpeg", "avif", "webp", "gif", "bmp"]);
function isImageFilename(name) {
  const lower = (name || "").toLowerCase();
  const dot = lower.lastIndexOf(".");
  if (dot < 0) return false;
  return IMAGE_EXTS.has(lower.slice(dot + 1));
}

// Render an 8-channel punched-tape visual from the filename (holes
// represent the actual ASCII bytes). Looping SMIL scroll for the retro
// vibe - pure decoration, no modem semantics. Placed in lieu of the
// image in file mode.
function renderFileTape(filename) {
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

// Single state for the AVIF controls (resize / quality / speed): locked
// when the source is already an AVIF (passthrough) OR not an image
// (zstd). In both cases, these controls have no effect on emitted bytes.
function applyTxModeUI() {
  const passthrough = !!txState.avifPassthrough;
  const file = !!txState.fileMode;
  const lock = passthrough || file;
  const hint = document.getElementById("tx-passthrough-hint");
  if (hint) {
    if (file) {
      hint.hidden = false;
      hint.textContent = "Fichier non-image → compression zstd sans perte";
    } else if (passthrough) {
      hint.hidden = false;
      hint.textContent = "AVIF natif → passthrough (pas de ré-encodage)";
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
// Aliases for compatibility with existing call sites.
function applyPassthroughUI() { applyTxModeUI(); }
function applyFileModeUI() { applyTxModeUI(); }

// Show the busy overlay immediately, before any async work. Pinning
// it on the preview area at the start of the load (rather than waiting
// for `_runTxCompressImpl` to add `.compressing` after the 50ms
// debounce + Promise chain) is what keeps the spinner from flashing
// for a single frame on fast images.
function showTxBusyOverlay() {
  const drop = document.getElementById("tx-drop-zone");
  const preview = document.getElementById("tx-preview");
  if (drop) drop.hidden = true;
  if (preview) {
    preview.hidden = false;
    preview.classList.add("compressing");
  }
}
function hideTxBusyOverlay() {
  const preview = document.getElementById("tx-preview");
  if (preview) preview.classList.remove("compressing");
}

// Load a file from a disk path (native Tauri drag-drop). The backend
// reads the bytes itself via set_tx_source_from_path: we completely
// avoid JSON-array IPC serialization which, on a large image, allocated
// ~10x the file size on both JS and Rust sides and could freeze KDE.
async function loadTxFileFromPath(path) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  // Anti-reentrance: ignore successive drops while a load OR compression
  // is in progress. Without the `compressing` check, dropping a new image
  // during a long ravif encode replaced the backend tx_source mid-flight
  // and piled `_runTxCompressImpl` calls on `_compressChain` until the
  // WebView ran out of memory.
  if (txState.loading || txState.compressing) {
    logEvent("tx_drop_ignored", { message: "chargement ou compression déjà en cours", path });
    return;
  }
  txState.loading = true;
  showTxBusyOverlay();
  const { convertFileSrc, invoke } = window.__TAURI__.core;
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
    txState.lastTx = null;
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

async function loadTxFile(file) {
  if (!file) return;
  // Anti-reentrance: same rationale as `loadTxFileFromPath` — without
  // this guard, picking a new image during a long ravif encode crashed
  // the WebView via piled-up `_compressChain` impls.
  if (txState.loading || txState.compressing) {
    logEvent("tx_pick_ignored", { message: "chargement ou compression déjà en cours" });
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
    txState.lastTx = null;
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
      const { invoke } = window.__TAURI__.core;
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

async function resetTxFile() {
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
    const { invoke } = window.__TAURI__.core;
    await invoke("clear_tx_source");
  } catch {
    // Doesn't matter: the JS state is already reset.
  }
}

function setupTxTab() {
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
    const { listen } = window.__TAURI__.event;
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
    txState.lastTx = null;
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
    if (v <= 2) return "très lent · meilleure compression";
    if (v <= 4) return "lent · bonne compression";
    if (v <= 6) return "équilibré";
    if (v <= 8) return "rapide · fichier plus gros";
    return "très rapide · fichier + gros";
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

// ────────────────────────────────────────────── TX orchestration (RX↔TX)
async function txStart() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  if (txState.txActive) return;
  if (!txState.estimate) {
    logEvent("tx_start_skipped", { reason: "pas d'estimation (compresse d'abord)" });
    return;
  }
  const { invoke } = window.__TAURI__.core;
  const rxStopBtn = document.getElementById("btn-stop");
  const rxWasActive = rxStopBtn && !rxStopBtn.disabled;
  if (rxWasActive) {
    try {
      await invoke("stop_capture");
    } catch (err) {
      logEvent("tx_pre_stop_error", { message: String(err) });
    }
  }
  txState.restartRxAfter = rxWasActive;
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
  try {
    await invoke("tx_start", {
      args: {
        mode: txState.mode,
        callsign: currentSettings.callsign || "",
        filename: getTxFilename(),
        tx_device: currentSettings.tx_device || "",
        repair_pct: txState.repairPct,
      },
    });
    // After an initial TX, we remember the session state to enable "More".
    // The initial burst emits K + floor(K * pct / 100) packets (cf. CLI
    // main.rs, Rust integer division). Must match exactly to avoid an
    // ESI gap between the initial burst and the first More.
    const k = computeK();
    if (k) {
      const pct = txState.repairPct || 0;
      const emitted = k + Math.floor((k * pct) / 100);
      txState.lastTx = { mode: txState.mode, esiMax: emitted - 1 };
    }
  } catch (err) {
    logEvent("tx_start_error", { message: String(err) });
    txState.txActive = false;
    refreshTxButtons();
    await maybeRestartRx();
  }
}

// K RaptorQ = number of source codewords required for decoding.
// Provided directly by the backend via the estimate (k_source), or
// approximated through total_blocks for compatibility with an older
// backend.
function computeK() {
  const est = txState.estimate;
  if (!est) return null;
  if (est.k_source != null) return Math.max(4, est.k_source);
  if (est.total_blocks != null) return Math.max(4, est.total_blocks);
  return null;
}

// Number of additional blocks to emit in a "More" burst. Read directly
// from the numeric input (presets via datalist, free input allowed).
function computeMoreCount() {
  const el = document.getElementById("tx-more-count");
  if (!el) return txState.moreCount || 5;
  const v = parseInt(el.value, 10);
  return Number.isFinite(v) && v > 0 ? v : (txState.moreCount || 5);
}

async function txMore() {
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
  const { invoke } = window.__TAURI__.core;
  const rxStopBtn = document.getElementById("btn-stop");
  const rxWasActive = rxStopBtn && !rxStopBtn.disabled;
  if (rxWasActive) {
    try {
      await invoke("stop_capture");
    } catch (err) {
      logEvent("tx_pre_stop_error", { message: String(err) });
    }
  }
  txState.restartRxAfter = rxWasActive;
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
        callsign: currentSettings.callsign || "",
        filename: getTxFilename(),
        tx_device: currentSettings.tx_device || "",
        esi_start: esiStart,
        count: count,
      },
    });
  } catch (err) {
    logEvent("tx_more_error", { message: String(err) });
    txState.txActive = false;
    refreshTxButtons();
    await maybeRestartRx();
  }
}

async function txStop() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  try {
    await invoke("tx_stop");
  } catch (err) {
    logEvent("tx_stop_error", { message: String(err) });
  }
}

async function maybeRestartRx() {
  if (!txState.restartRxAfter) return;
  txState.restartRxAfter = false;
  // Small delay to let the TX sound card release its handles before
  // opening the RX capture (especially when the same card is used).
  await new Promise((r) => setTimeout(r, 300));
  await startCapture();
}

function updateTxProgressText() {
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
      const durK = est.duration_s_k != null ? ` (seuil K : ${fmtSeconds(est.duration_s_k)})` : "";
      txt.textContent = `— / ${n} blocs · ${k} nécessaires · durée ~${dur}${durK}`;
    } else {
      txt.textContent = "—";
    }
    return;
  }
  const kTail = est && est.k_source != null ? ` · K=${est.k_source}` : "";
  txt.textContent =
    `TX ${p.blocks_sent} / ${p.total_blocks} blocs${kTail} · ${fmtSeconds(p.elapsed_s)} / ${fmtSeconds(p.duration_s)}`;
}

function onTxProgress(payload) {
  txState.progress = payload;
  updateTxProgressText();
  // Reuse the bottom progress bar (blocks) in TX mode.
  const bitmap = new Uint8Array(Math.ceil((payload.total_blocks || 0) / 8));
  for (let i = 0; i < payload.blocks_sent; i++) {
    bitmap[i >> 3] |= 1 << (i & 7);
  }
  lastProgress = {
    bitmap,
    expected: payload.total_blocks,
    converged: payload.blocks_sent,
    sigma2: null,
  };
  drawProgressBlocks();
}

async function onTxComplete(payload) {
  logEvent("tx_complete", payload);
  txState.txActive = false;
  txState.progress = null;
  updateTxProgressText();
  refreshTxButtons();
  try {
    const { invoke } = window.__TAURI__.core;
    await invoke("tx_reset");
  } catch (_) {}
  // Reset affichage RX + relance si besoin.
  resetRxVisuals();
  await maybeRestartRx();
}

async function onTxError(payload) {
  logEvent("tx_error", payload);
  txState.txActive = false;
  txState.progress = null;
  updateTxProgressText();
  refreshTxButtons();
  try {
    const { invoke } = window.__TAURI__.core;
    await invoke("tx_reset");
  } catch (_) {}
  resetRxVisuals();
  await maybeRestartRx();
}

// ─────────────────────────────────────────── Channel tab (cascade ATT)
// Phase A: a single persistent setting (tx_attenuation_db in Settings),
// fed either manually via the slider or by the median of a list of
// feedbacks received during QSO. Cascade list: JS session only.
let cascadeFeedback = [];

function attGainStr(db) {
  const lin = Math.pow(10, db / 20);
  return `×${lin.toFixed(3)} (${db.toFixed(1)} dB)`;
}

function clampAttDb(v) {
  if (!Number.isFinite(v)) return 0;
  if (v > 0) return 0;
  if (v < -30) return -30;
  return v;
}

function syncAttUi(db) {
  const slider = document.getElementById("att-slider");
  const input = document.getElementById("att-input");
  const info = document.getElementById("att-gain-info");
  if (slider) slider.value = String(db);
  if (input) input.value = String(db);
  if (info) info.textContent = attGainStr(db);
}

async function applyAttenuation(db, source) {
  const v = clampAttDb(db);
  currentSettings.tx_attenuation_db = v;
  syncAttUi(v);
  const status = document.getElementById("att-status");
  try {
    if (window.__TAURI__ && window.__TAURI__.core) {
      await window.__TAURI__.core.invoke("save_settings", { settings: currentSettings });
    }
    if (status) {
      status.textContent = source
        ? `${source} → ${v.toFixed(1)} dB sauvegardé ${now()}`
        : `${v.toFixed(1)} dB sauvegardé ${now()}`;
    }
  } catch (err) {
    if (status) status.textContent = `erreur : ${err}`;
  }
}

function median(values) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

function mean(values) {
  if (values.length === 0) return null;
  return values.reduce((a, b) => a + b, 0) / values.length;
}

function renderCascade() {
  const tbody = document.getElementById("cascade-tbody");
  const medEl = document.getElementById("cascade-median");
  const meanEl = document.getElementById("cascade-mean");
  const apply = document.getElementById("cascade-apply");
  if (!tbody) return;
  if (cascadeFeedback.length === 0) {
    tbody.innerHTML = `<tr><td colspan="3" class="cascade-empty">Aucun rapport.</td></tr>`;
    if (medEl) medEl.textContent = "—";
    if (meanEl) meanEl.textContent = "—";
    if (apply) apply.disabled = true;
    return;
  }
  tbody.innerHTML = cascadeFeedback
    .map((row, i) =>
      `<tr><td>${escapeHtml(row.call)}</td><td>${row.db.toFixed(1)}</td>` +
      `<td><button class="cascade-row-del" data-idx="${i}" title="Supprimer">✕</button></td></tr>`
    )
    .join("");
  for (const btn of tbody.querySelectorAll(".cascade-row-del")) {
    btn.addEventListener("click", (ev) => {
      const idx = Number(ev.currentTarget.dataset.idx);
      if (Number.isFinite(idx)) {
        cascadeFeedback.splice(idx, 1);
        renderCascade();
      }
    });
  }
  const vals = cascadeFeedback.map(r => r.db);
  if (medEl) medEl.textContent = `${median(vals).toFixed(1)} dB`;
  if (meanEl) meanEl.textContent = `${mean(vals).toFixed(1)} dB`;
  if (apply) apply.disabled = false;
}

function setupChannelTab() {
  const slider = document.getElementById("att-slider");
  const input = document.getElementById("att-input");
  const reset = document.getElementById("att-reset");
  const initialDb = clampAttDb(Number(currentSettings.tx_attenuation_db) || 0);
  syncAttUi(initialDb);
  if (slider) {
    slider.addEventListener("input", () => {
      const v = clampAttDb(Number(slider.value));
      syncAttUi(v);
    });
    slider.addEventListener("change", () => {
      applyAttenuation(Number(slider.value), "slider");
    });
  }
  if (input) {
    input.addEventListener("change", () => {
      applyAttenuation(Number(input.value), "saisie");
    });
  }
  if (reset) {
    reset.addEventListener("click", () => applyAttenuation(0, "reset"));
  }
  const callInput = document.getElementById("cascade-call");
  const dbInput = document.getElementById("cascade-db");
  const addBtn = document.getElementById("cascade-add");
  const applyBtn = document.getElementById("cascade-apply");
  const clearBtn = document.getElementById("cascade-clear");
  function addCascadeEntry() {
    const call = (callInput && callInput.value || "").trim().toUpperCase();
    const db = Number(dbInput && dbInput.value);
    if (!Number.isFinite(db)) return;
    cascadeFeedback.push({ call: call || "?", db });
    if (callInput) callInput.value = "";
    if (dbInput) dbInput.value = "";
    if (callInput) callInput.focus();
    renderCascade();
  }
  if (addBtn) addBtn.addEventListener("click", addCascadeEntry);
  for (const el of [callInput, dbInput]) {
    if (!el) continue;
    el.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        ev.preventDefault();
        addCascadeEntry();
      }
    });
  }
  if (applyBtn) {
    applyBtn.addEventListener("click", () => {
      const vals = cascadeFeedback.map(r => r.db);
      const m = median(vals);
      if (m !== null) applyAttenuation(m, "médiane cascade");
    });
  }
  if (clearBtn) {
    clearBtn.addEventListener("click", () => {
      cascadeFeedback = [];
      renderCascade();
    });
  }
  renderCascade();
}

// ─────────────────────────────────────────── History tab
// Unified TX (files emitted, archived at each tx_start) and RX (decoded
// sessions) view. "↻ Relay" button on each thumbnail for the emergency-
// radio mode: reload a file in the TX tab and propagate it further on
// the network.

function setupHistoryTab() {
  document
    .getElementById("btn-history-refresh")
    ?.addEventListener("click", refreshHistory);
}

async function refreshHistory() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  try {
    const [tx, rx] = await Promise.all([
      invoke("list_tx_history"),
      invoke("list_rx_history"),
    ]);
    renderHistoryColumn(tx, "tx");
    renderHistoryColumn(rx, "rx");
    const cnt = document.getElementById("history-count");
    if (cnt) cnt.textContent = `TX ${tx.length} · RX ${rx.length}`;
  } catch (err) {
    logEvent("history_error", { message: String(err) });
  }
}

function renderHistoryColumn(items, kind) {
  const list = document.getElementById(`history-${kind}-list`);
  if (!list) return;
  list.innerHTML = "";
  if (!items.length) {
    const empty = document.createElement("div");
    empty.className = "history-empty";
    empty.textContent = kind === "tx" ? "Aucun fichier émis." : "Aucun fichier reçu.";
    list.appendChild(empty);
    return;
  }
  const { convertFileSrc } = window.__TAURI__.core;
  for (const item of items) {
    const card = document.createElement("div");
    card.className = "history-card";

    // Thumbnail (image or file icon).
    const thumb = document.createElement("div");
    thumb.className = "history-card-thumb";
    const previewPath = kind === "tx" ? item.file_path : item.preview_path;
    if (item.is_image) {
      const img = document.createElement("img");
      img.alt = item.filename;
      img.src = convertFileSrc(previewPath);
      img.addEventListener("dblclick", () =>
        openLightbox(convertFileSrc(previewPath), item.filename),
      );
      thumb.addEventListener("click", () =>
        openLightbox(convertFileSrc(previewPath), item.filename),
      );
      thumb.appendChild(img);
    } else {
      const icon = document.createElement("div");
      icon.className = "file-icon";
      icon.textContent = "📄";
      thumb.appendChild(icon);
      const fname = document.createElement("div");
      fname.className = "file-name";
      fname.textContent = item.filename;
      thumb.appendChild(fname);
      thumb.style.cursor = "default";
    }
    card.appendChild(thumb);

    // Bandeau metadata.
    const meta = document.createElement("div");
    meta.className = "history-card-meta";
    const row1 = document.createElement("div");
    row1.className = "row";
    const fname = document.createElement("span");
    fname.className = "filename";
    fname.title = item.filename;
    fname.textContent = item.filename;
    row1.appendChild(fname);
    const mode = document.createElement("span");
    mode.className = "mode";
    mode.textContent = item.mode;
    row1.appendChild(mode);
    meta.appendChild(row1);
    const row2 = document.createElement("div");
    row2.className = "row";
    const ts = document.createElement("span");
    ts.className = "ts";
    ts.textContent = formatTimestamp(item.timestamp);
    row2.appendChild(ts);
    if (kind === "rx" && item.callsign) {
      const cs = document.createElement("span");
      cs.className = "callsign";
      cs.textContent = item.callsign;
      row2.appendChild(cs);
    }
    const sz = document.createElement("span");
    sz.className = "size";
    sz.textContent = formatBytes(item.size_bytes);
    row2.appendChild(sz);
    meta.appendChild(row2);
    card.appendChild(meta);

    // Actions : ↻ Renvoyer (TX & RX = relayage radio-secours) + 🗑 Supprimer.
    const actions = document.createElement("div");
    actions.className = "history-card-actions";
    const relayBtn = document.createElement("button");
    relayBtn.className = "btn-relay";
    relayBtn.textContent = kind === "tx" ? "↻ Renvoyer" : "↻ Relayer";
    relayBtn.title =
      kind === "tx"
        ? "Recharger ce fichier dans l'onglet TX"
        : "Relayer ce fichier reçu (radio-secours)";
    const relayPath = kind === "tx" ? item.file_path : item.relay_path;
    relayBtn.addEventListener("click", () => relayHistoryItem(relayPath));
    actions.appendChild(relayBtn);

    const delBtn = document.createElement("button");
    delBtn.className = "btn-delete";
    delBtn.textContent = "🗑";
    delBtn.title = "Supprimer cette entrée";
    delBtn.addEventListener("click", () => {
      const label = item.filename || "cette entrée";
      if (!confirm(`Supprimer ${label} de l'historique ?`)) return;
      const key = kind === "tx" ? item.file_path : item.session_id;
      deleteHistoryItem(kind, key);
    });
    actions.appendChild(delBtn);
    card.appendChild(actions);

    list.appendChild(card);
  }
}

async function relayHistoryItem(absolutePath) {
  // Bascule sur l'onglet TX puis recharge le fichier comme un drag-drop.
  const txBtn = document.querySelector('.tab-bar .tab[data-tab="tx"]');
  if (txBtn) txBtn.click();
  try {
    await loadTxFileFromPath(absolutePath);
  } catch (err) {
    logEvent("history_relay_error", { path: absolutePath, message: String(err) });
  }
}

async function deleteHistoryItem(kind, key) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke } = window.__TAURI__.core;
  try {
    await invoke("delete_history_item", { kind, key });
    await refreshHistory();
  } catch (err) {
    logEvent("history_delete_error", { kind, key, message: String(err) });
    alert(`Suppression impossible : ${err}`);
  }
}

function formatTimestamp(unixSeconds) {
  if (!unixSeconds) return "—";
  const d = new Date(unixSeconds * 1000);
  return d.toLocaleString("fr-CH", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatBytes(n) {
  if (!n || n < 1024) return `${n || 0} o`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} Kio`;
  return `${(n / (1024 * 1024)).toFixed(2)} Mio`;
}

// Kiosk mode (small touchscreen, e.g. Pi 7" 800x480) — the Rust setup
// hook auto-engages fullscreen and emits `kiosk_mode` so the frontend
// can switch its CSS layout and reveal the on-screen exit button.
function setupKioskMode() {
  // Add the body class based on viewport size — independent of any
  // Rust-side event. The Rust setup hook emits `kiosk_mode` *before*
  // the webview is loaded so the listener-driven path always loses
  // the race; we still register the listener for completeness (e.g.
  // when an event fires later from runtime), but the viewport check
  // is what actually drives the class on a fresh open.
  if (window.innerWidth <= 900 || window.innerHeight <= 600) {
    document.body.classList.add("kiosk-mode");
  }
  if (window.__TAURI__ && window.__TAURI__.event) {
    window.__TAURI__.event.listen("kiosk_mode", () => {
      document.body.classList.add("kiosk-mode");
    });
  }
  const exitBtn = document.getElementById("kiosk-exit");
  if (exitBtn && window.__TAURI__ && window.__TAURI__.window) {
    exitBtn.addEventListener("click", async () => {
      try {
        await window.__TAURI__.window.getCurrentWindow().close();
      } catch (e) {
        console.error("kiosk close", e);
      }
    });
  }
  // Escape toggles fullscreen on/off (kiosk mode only). The image
  // lightbox owns its own Escape handler and we yield to it when open.
  window.addEventListener("keydown", async (ev) => {
    if (ev.key !== "Escape") return;
    if (!document.body.classList.contains("kiosk-mode")) return;
    const lb = document.getElementById("image-lightbox");
    if (lb && !lb.hidden) return;
    if (!window.__TAURI__ || !window.__TAURI__.window) return;
    try {
      const win = window.__TAURI__.window.getCurrentWindow();
      const isFs = await win.isFullscreen();
      await win.setFullscreen(!isFs);
    } catch (e) {
      console.error("kiosk toggle", e);
    }
  });
}

async function init() {
  setupKioskMode();
  setupSelectPicker();
  setupVirtKeyboard();
  setupTabs();
  setupLightbox();
  setupTxTab();
  setupSettingsTab();
  setupOverlaysTab();
  setupCaptureSubmitPanel();
  setupHistoryTab();
  await loadSettings();
  applyOverlaysToUI();
  setupChannelTab();
  await loadDevices();
  await loadSerialPorts();
  await loadSaveDir();
  // Display the initial PTT state (computed by the backend at setup).
  try {
    const st = await window.__TAURI__.core.invoke("ptt_status");
    renderPttStatus(st);
  } catch (err) {
    console.error("ptt_status", err);
  }
  wireEvents();
  document.getElementById("btn-start").addEventListener("click", startCapture);
  document.getElementById("btn-stop").addEventListener("click", stopCapture);
  document.getElementById("btn-raw").addEventListener("click", toggleRawRecording);
  window.addEventListener("resize", redrawAll);
  document
    .getElementById("btn-sessions-refresh")
    ?.addEventListener("click", refreshSessions);
  await refreshRawRecordingState();
  await refreshSessions();
  resetRxVisuals();
  // #HB9TOB: periodic tick to clear the OVD chip if no overdrive batch
  // has arrived for OVD_STICKY_MS (also useful when capture is stopped).
  setInterval(refreshOverdriveChip, 200);
  // Auto-start RX capture if a device is configured.
  await tryAutoStartCapture();
}

// ─── Touch-friendly <select> picker (kiosk) ───────────────────────
//
// WebKitGTK on Wayland renders `<select>` as a native popup whose
// height is capped to ~5-6 rows on the 800x480 Pi DSI panel. With
// 10 entries in `tx-mode` / `rx-forced-profile` the bottom of the
// list (where the experimental profiles live) is unreachable on a
// touchscreen — the popup is scrollable in theory but the affordance
// is invisible, so the user thinks the experimentals are gone.
//
// Fix: in kiosk mode we capture `mousedown` on every `<select>`
// before the engine opens its popup, and present a fullscreen modal
// instead — same options, ≥48 px tap targets, scroll obvious. Off
// kiosk this stays dormant, so the desktop UX is untouched.
//
// Opt-out: `data-select-picker-skip="1"` on the `<select>`.

const selectPicker = {
  modal: null,
  labelEl: null,
  listEl: null,
  closeEl: null,
  target: null,
};

function setupSelectPicker() {
  selectPicker.modal = document.getElementById("select-picker-modal");
  selectPicker.labelEl = document.getElementById("select-picker-label");
  selectPicker.listEl = document.getElementById("select-picker-list");
  selectPicker.closeEl = document.getElementById("select-picker-cancel");
  if (!selectPicker.modal) return;

  // Capture phase — must run before WebKitGTK opens its native popup.
  // Mousedown is the right hook: click is fired AFTER the native
  // popup has already opened (and consumed the gesture on touch).
  document.addEventListener("mousedown", (e) => {
    if (!document.body.classList.contains("kiosk-mode")) return;
    let el = e.target;
    if (el instanceof HTMLOptionElement) el = el.parentElement;
    if (!(el instanceof HTMLSelectElement)) return;
    if (el.disabled) return;
    if (el.dataset.selectPickerSkip === "1") return;
    if (selectPicker.modal.contains(el)) return;
    e.preventDefault();
    e.stopPropagation();
    el.blur();
    openSelectPicker(el);
  }, /*capture=*/true);

  // Same hook on `keydown` Space/Enter, in case the select is
  // reached by Tab (the kiosk has no keyboard, but the desktop devs
  // can still keyboard-navigate).
  document.addEventListener("keydown", (e) => {
    if (!document.body.classList.contains("kiosk-mode")) return;
    if (e.key !== " " && e.key !== "Enter" && e.key !== "ArrowDown") return;
    const el = e.target;
    if (!(el instanceof HTMLSelectElement)) return;
    if (el.disabled || el.dataset.selectPickerSkip === "1") return;
    e.preventDefault();
    openSelectPicker(el);
  }, /*capture=*/true);

  selectPicker.closeEl.addEventListener("click", closeSelectPicker);
  selectPicker.modal.addEventListener("click", (e) => {
    if (e.target === selectPicker.modal) closeSelectPicker();
  });
  document.addEventListener("keydown", (e) => {
    if (selectPicker.modal.hidden) return;
    if (e.key === "Escape") {
      e.preventDefault();
      closeSelectPicker();
    }
  });
}

function selectLabel(sel) {
  // Surrounding <label>'s text minus the <select>'s current option text,
  // falls back to <legend>, then to the id. Matches the virtKb pattern.
  const lab = sel.closest("label");
  if (lab) {
    const txt = (lab.textContent || "")
      .replace(sel.options[sel.selectedIndex]?.textContent || "", "")
      .trim();
    if (txt) return txt.replace(/\s+/g, " ").slice(0, 80);
  }
  const fs = sel.closest("fieldset");
  const lg = fs && fs.querySelector("legend");
  if (lg && lg.textContent) return lg.textContent.trim().slice(0, 80);
  return sel.id || "Choisir";
}

function openSelectPicker(sel) {
  selectPicker.target = sel;
  selectPicker.labelEl.textContent = selectLabel(sel);
  const list = selectPicker.listEl;
  list.innerHTML = "";
  for (const opt of sel.options) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "select-picker-row";
    if (opt.classList.contains("experimental-option")) {
      row.classList.add("select-picker-experimental");
    }
    if (opt.value === sel.value) {
      row.classList.add("select-picker-current");
    }
    if (opt.disabled) row.classList.add("select-picker-disabled");
    row.dataset.value = opt.value;
    row.textContent = opt.textContent;
    if (opt.disabled) row.disabled = true;
    row.addEventListener("click", () => {
      const target = selectPicker.target;
      if (target && target.value !== opt.value) {
        target.value = opt.value;
        // Notify the rest of the app exactly as the native popup does.
        target.dispatchEvent(new Event("input", { bubbles: true }));
        target.dispatchEvent(new Event("change", { bubbles: true }));
      }
      closeSelectPicker();
    });
    list.appendChild(row);
  }
  selectPicker.modal.hidden = false;
  // Scroll the current selection into view so opening on a long list
  // (10 modem profiles, 39 CTCSS tones) doesn't always start from top.
  const cur = list.querySelector(".select-picker-current");
  if (cur) cur.scrollIntoView({ block: "center" });
}

function closeSelectPicker() {
  if (!selectPicker.modal || selectPicker.modal.hidden) return;
  selectPicker.modal.hidden = true;
  selectPicker.target = null;
  selectPicker.listEl.innerHTML = "";
}

// ─── Native virtual keyboard (kiosk text/number entry) ────────────
//
// In kiosk mode (no physical keyboard on the 7" Pi panel) text/number
// inputs become impossible to fill: the user can't enter their callsign,
// a filename, a Pluto frequency offset, etc. This component is a pure
// in-app touch keyboard — no system dependency (no squeekboard / wvkbd /
// onboard) — that auto-opens when an `<input>` is focused while
// `body.kiosk-mode` is set. Outside kiosk it stays dormant: a physical
// keyboard typing into the input works like before.
//
// Layouts:
//   * `alpha`   QWERTY uppercase + lower toggle, with shift, space,
//               and a `?123` key to switch to symbols/numerics.
//   * `symbols` digits, common punctuation, `ABC` to come back.
//   * `numeric` 0-9 + decimal point + sign for `<input type="number">`.
//
// Special-cased by input id:
//   * `callsign-input` opens caps-locked, ASCII letters + digits only.
//
// On Valider: write `virtKb.draft` to `target.value`, dispatch `input`
// + `change`, close. On Annuler / outside-tap / Esc: close, no save.
const VIRT_KB_QWERTY_ROWS = [
  ["q","w","e","r","t","y","u","i","o","p"],
  ["a","s","d","f","g","h","j","k","l"],
  ["z","x","c","v","b","n","m"],
];
const VIRT_KB_SYMBOLS_ROWS = [
  ["1","2","3","4","5","6","7","8","9","0"],
  ["@","#","_","-",".",",","/",":","="],
  ["+","*","(",")","'","\"","?","!","%"],
];
const VIRT_KB_NUMERIC_ROWS = [
  ["1","2","3"],
  ["4","5","6"],
  ["7","8","9"],
  ["-",".","0"],
];

const virtKb = {
  modal: null,
  rowsEl: null,
  displayEl: null,
  labelEl: null,
  okBtn: null,
  cancelBtn: null,
  closeBtn: null,
  // Live state — reset on every open():
  target: null,        // the original <input> element
  draft: "",           // editable string buffer
  layout: "alpha",     // "alpha" | "symbols" | "numeric"
  shift: false,        // true = uppercase letters in alpha mode
  capsLock: false,     // sticky shift (callsign field auto-engages)
};

function setupVirtKeyboard() {
  virtKb.modal = document.getElementById("virt-keyboard-modal");
  virtKb.rowsEl = document.getElementById("virt-kb-rows");
  virtKb.displayEl = document.getElementById("virt-kb-value");
  virtKb.labelEl = document.getElementById("virt-kb-label");
  virtKb.okBtn = document.getElementById("virt-kb-ok");
  virtKb.cancelBtn = document.getElementById("virt-kb-cancel-btn");
  virtKb.closeBtn = document.getElementById("virt-kb-cancel");
  if (!virtKb.modal) return;

  // Single delegated focusin handler (cheap, no re-attach when DOM
  // changes). Filters by input.type so checkboxes / radios / file pickers
  // don't trigger the keyboard. Hidden inputs in modals (e.g. file
  // picker) are skipped via `:not([hidden])`.
  document.addEventListener("focusin", (e) => {
    if (!document.body.classList.contains("kiosk-mode")) return;
    const t = e.target;
    if (!(t instanceof HTMLInputElement)) return;
    if (t.dataset.virtKbSkip === "1") return;
    const type = (t.type || "text").toLowerCase();
    if (type !== "text" && type !== "number" && type !== "search" && type !== "tel" && type !== "url") return;
    // Already inside the keyboard? avoid recursion (the display is a
    // <span>, not an input, but defensive).
    if (virtKb.modal.contains(t)) return;
    // Unfocus the input so the OS soft-keyboard (if any) doesn't try to
    // appear underneath. We keep a reference for the commit.
    t.blur();
    openVirtKeyboard(t);
  });

  // OK / Cancel / outside-tap / Esc.
  virtKb.okBtn.addEventListener("click", () => closeVirtKeyboard(true));
  virtKb.cancelBtn.addEventListener("click", () => closeVirtKeyboard(false));
  virtKb.closeBtn.addEventListener("click", () => closeVirtKeyboard(false));
  virtKb.modal.addEventListener("click", (e) => {
    if (e.target === virtKb.modal) closeVirtKeyboard(false);
  });
  document.addEventListener("keydown", (e) => {
    if (virtKb.modal.hidden) return;
    if (e.key === "Escape") {
      e.preventDefault();
      closeVirtKeyboard(false);
    } else if (e.key === "Enter") {
      e.preventDefault();
      closeVirtKeyboard(true);
    }
  });
}

function openVirtKeyboard(input) {
  virtKb.target = input;
  virtKb.draft = input.value || "";
  // Pick layout from input.type and id.
  const type = (input.type || "text").toLowerCase();
  if (type === "number") {
    virtKb.layout = "numeric";
    virtKb.shift = false;
    virtKb.capsLock = false;
  } else {
    virtKb.layout = "alpha";
    // Callsign field — uppercase locked, ASCII alphanum only. Same
    // pragmatic behaviour as a real radio's callsign editor.
    virtKb.capsLock = (input.id === "callsign-input");
    virtKb.shift = virtKb.capsLock;
  }
  virtKb.labelEl.textContent = inputLabel(input);
  virtKb.modal.hidden = false;
  renderVirtKeyboardLayout();
  refreshVirtKbDisplay();
}

function closeVirtKeyboard(commit) {
  if (!virtKb.modal || virtKb.modal.hidden) return;
  const target = virtKb.target;
  if (commit && target) {
    const max = parseInt(target.getAttribute("maxlength") || "0", 10);
    let out = virtKb.draft;
    if (max > 0) out = out.slice(0, max);
    target.value = out;
    // Notify any change listener wired by the rest of the app
    // (persistSettings, refreshTxEstimate, …).
    target.dispatchEvent(new Event("input", { bubbles: true }));
    target.dispatchEvent(new Event("change", { bubbles: true }));
    // Frequency inputs : add the validated value to the MRU
    // favorites so the next keypad open offers it as a quick-pick.
    if (isFreqInputId(target.id)) {
      const mhz = parseFloat(out);
      if (Number.isFinite(mhz) && mhz > 0) {
        // fire-and-forget — own save flush, doesn't block close.
        pushFreqMru(mhz);
      }
    }
  }
  virtKb.modal.hidden = true;
  virtKb.target = null;
  virtKb.draft = "";
}

function inputLabel(input) {
  // Prefer the surrounding <label>'s direct text node, fall back to
  // placeholder, then to the id.
  const lab = input.closest("label");
  if (lab) {
    const txt = (lab.textContent || "").replace(input.value || "", "").trim();
    if (txt) return txt.replace(/\s+/g, " ").slice(0, 60);
  }
  if (input.placeholder) return input.placeholder;
  return input.id || "Saisie";
}

function renderVirtKeyboardLayout() {
  const rows = virtKb.rowsEl;
  rows.innerHTML = "";
  if (virtKb.layout === "numeric") {
    // Extra rows for frequency inputs: MRU favorites + step buttons.
    // Detected by input id; falls back to a plain numeric pad for any
    // other `<input type="number">` (gain dB, attenuation, etc).
    if (isFreqInputId(virtKb.target?.id)) {
      renderVirtKbFavoritesRow();
      renderVirtKbStepRow();
    }
    for (const row of VIRT_KB_NUMERIC_ROWS) {
      const r = document.createElement("div");
      r.className = "virt-kb-row";
      for (const k of row) r.appendChild(makeKbKey(k));
      rows.appendChild(r);
    }
    const trailer = document.createElement("div");
    trailer.className = "virt-kb-row";
    trailer.appendChild(makeKbKey("⌫", "back", "wide-2 special"));
    rows.appendChild(trailer);
    return;
  }
  const rowsData = virtKb.layout === "symbols" ? VIRT_KB_SYMBOLS_ROWS : VIRT_KB_QWERTY_ROWS;
  for (const row of rowsData) {
    const r = document.createElement("div");
    r.className = "virt-kb-row";
    for (const k of row) {
      const display = (virtKb.layout === "alpha" && (virtKb.shift || virtKb.capsLock))
        ? k.toUpperCase()
        : k;
      r.appendChild(makeKbKey(display, k));
    }
    rows.appendChild(r);
  }
  // Last row : layout-specific specials.
  const last = document.createElement("div");
  last.className = "virt-kb-row";
  if (virtKb.layout === "alpha") {
    const shiftLabel = virtKb.capsLock ? "⇪" : "⇧";
    last.appendChild(makeKbKey(shiftLabel, "shift", "wide-2 special"));
    last.appendChild(makeKbKey("?123", "to-symbols", "wide-2 special"));
    last.appendChild(makeKbKey("Esp.", "space", "wide-3 special"));
    last.appendChild(makeKbKey("⌫", "back", "wide-2 special"));
  } else {
    last.appendChild(makeKbKey("ABC", "to-alpha", "wide-2 special"));
    last.appendChild(makeKbKey("Esp.", "space", "wide-3 special"));
    last.appendChild(makeKbKey("⌫", "back", "wide-2 special"));
  }
  rows.appendChild(last);
}

function makeKbKey(label, action, extraClass) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "virt-kb-key" + (extraClass ? " " + extraClass : "");
  b.textContent = label;
  b.dataset.action = action || label;
  b.addEventListener("click", () => onVirtKbKey(b.dataset.action));
  return b;
}

function onVirtKbKey(action) {
  switch (action) {
    case "back":
      virtKb.draft = virtKb.draft.slice(0, -1);
      break;
    case "space":
      appendVirtKbChar(" ");
      break;
    case "shift":
      if (virtKb.shift && !virtKb.capsLock) {
        // 1st tap : shift on. 2nd consecutive tap : caps-lock.
        virtKb.capsLock = true;
      } else if (virtKb.capsLock) {
        // Tap while capsLock : turn everything off.
        virtKb.capsLock = false;
        virtKb.shift = false;
      } else {
        virtKb.shift = true;
      }
      renderVirtKeyboardLayout();
      return;
    case "to-symbols":
      virtKb.layout = "symbols";
      renderVirtKeyboardLayout();
      return;
    case "to-alpha":
      virtKb.layout = "alpha";
      renderVirtKeyboardLayout();
      return;
    default:
      // Single-character key: respect shift / capsLock for letters.
      let c = action;
      if (virtKb.layout === "alpha" && (virtKb.shift || virtKb.capsLock) && c.length === 1) {
        c = c.toUpperCase();
      }
      appendVirtKbChar(c);
      // Auto-release a one-shot shift (capsLock keeps it on).
      if (virtKb.shift && !virtKb.capsLock) {
        virtKb.shift = false;
        renderVirtKeyboardLayout();
      }
      break;
  }
  refreshVirtKbDisplay();
}

function appendVirtKbChar(c) {
  if (virtKb.target) {
    const max = parseInt(virtKb.target.getAttribute("maxlength") || "0", 10);
    if (max > 0 && virtKb.draft.length >= max) return;
  }
  virtKb.draft += c;
}

function refreshVirtKbDisplay() {
  // Replace ` ` to keep an empty draft visible (otherwise the
  // height collapses).
  virtKb.displayEl.textContent = virtKb.draft.length ? virtKb.draft : " ";
}

// ─── Frequency-keypad enrichments (MRU favorites + step buttons) ──
//
// Active only when the focused input is a Pluto frequency field
// (id starts with `pluto-rx-freq-` or `pluto-tx-freq-`). The MRU
// row mirrors `currentSettings.pluto_freq_favorites` (Hz, capped
// at 6, most-recent-first). Step buttons add/subtract a multiple
// of kHz to the displayed MHz value, picked from a 5 / 6.25 / 12.5
// / 25 kHz selector — the standard channel rasters in amateur and
// PMR repeater plans. The pick survives keypad close/re-open via
// `virtKb.stepKHz`.
const FREQ_INPUT_ID_PREFIX = ["pluto-rx-freq-", "pluto-tx-freq-"];
const STEP_OPTIONS_KHZ = [5.0, 6.25, 12.5, 25.0];
virtKb.stepKHz = 6.25;

function isFreqInputId(id) {
  if (!id) return false;
  return FREQ_INPUT_ID_PREFIX.some(p => id.startsWith(p));
}

function renderVirtKbFavoritesRow() {
  const favs = (currentSettings && Array.isArray(currentSettings.pluto_freq_favorites))
    ? currentSettings.pluto_freq_favorites : [];
  if (favs.length === 0) return;
  const row = document.createElement("div");
  row.className = "virt-kb-row virt-kb-favs";
  for (const hz of favs) {
    const mhz = (hz / 1e6).toFixed(3);
    const b = document.createElement("button");
    b.type = "button";
    b.className = "virt-kb-key special";
    b.textContent = mhz;
    b.title = `Charger ${mhz} MHz`;
    b.addEventListener("click", () => {
      // Strip trailing zeros so "145.500" displays compact, but
      // keep the decimal point so the user knows it's fractional.
      virtKb.draft = mhz.replace(/0+$/, "").replace(/\.$/, "");
      refreshVirtKbDisplay();
    });
    row.appendChild(b);
  }
  virtKb.rowsEl.appendChild(row);
}

function renderVirtKbStepRow() {
  const row = document.createElement("div");
  row.className = "virt-kb-row virt-kb-step-row";
  // Step selector — taps cycle through STEP_OPTIONS_KHZ.
  const stepBtn = document.createElement("button");
  stepBtn.type = "button";
  stepBtn.className = "virt-kb-key special wide-2";
  stepBtn.textContent = `Pas: ${virtKb.stepKHz} kHz`;
  stepBtn.title = "Cycle 5 / 6.25 / 12.5 / 25 kHz";
  stepBtn.addEventListener("click", () => {
    const idx = STEP_OPTIONS_KHZ.indexOf(virtKb.stepKHz);
    virtKb.stepKHz = STEP_OPTIONS_KHZ[(idx + 1) % STEP_OPTIONS_KHZ.length];
    stepBtn.textContent = `Pas: ${virtKb.stepKHz} kHz`;
  });
  row.appendChild(stepBtn);

  const minusBtn = document.createElement("button");
  minusBtn.type = "button";
  minusBtn.className = "virt-kb-key";
  minusBtn.textContent = "−";
  minusBtn.addEventListener("click", () => stepDraft(-1));
  row.appendChild(minusBtn);

  const plusBtn = document.createElement("button");
  plusBtn.type = "button";
  plusBtn.className = "virt-kb-key";
  plusBtn.textContent = "+";
  plusBtn.addEventListener("click", () => stepDraft(+1));
  row.appendChild(plusBtn);

  virtKb.rowsEl.appendChild(row);
}

function stepDraft(direction) {
  // Parse the current draft as MHz (accept partials like "145." or
  // ""). Empty / unparseable → start from 0.
  const cur = parseFloat(virtKb.draft);
  const start = Number.isFinite(cur) ? cur : 0;
  const deltaMHz = (virtKb.stepKHz / 1000.0) * direction;
  const next = start + deltaMHz;
  // 5 decimals = 10 Hz precision, more than enough for amateur
  // channel rasters. Trim trailing zeros for compact display.
  let fixed = next.toFixed(5).replace(/0+$/, "").replace(/\.$/, "");
  if (fixed === "" || fixed === "-") fixed = "0";
  virtKb.draft = fixed;
  refreshVirtKbDisplay();
}

/// Push the freshly-validated frequency (MHz) onto the MRU. Dedup
/// on Hz equality, prepend, cap at 6. Persists via save_settings
/// so the next keypad open already shows it.
async function pushFreqMru(mhz) {
  if (!Number.isFinite(mhz) || mhz <= 0) return;
  const hz = Math.round(mhz * 1e6);
  const list = Array.isArray(currentSettings.pluto_freq_favorites)
    ? currentSettings.pluto_freq_favorites.slice() : [];
  const idx = list.indexOf(hz);
  if (idx !== -1) list.splice(idx, 1);
  list.unshift(hz);
  while (list.length > 6) list.pop();
  currentSettings.pluto_freq_favorites = list;
  if (window.__TAURI__ && window.__TAURI__.core) {
    try {
      await window.__TAURI__.core.invoke("save_settings", { settings: currentSettings });
    } catch (err) {
      console.warn("save favorites:", err);
    }
  }
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
