// NBFM Modem GUI — 3-tab layout (RX / TX / Info) with per-block progress and
// live constellation display.

const MIME_TYPES = {
  0: "application/octet-stream",
  1: "image/avif",
  2: "image/jpeg",
  3: "image/png",
  4: "image/webp",
  5: "text/plain",
};

function mimeToExt(code) {
  return MIME_TYPES[code] || "application/octet-stream";
}

function isImageMime(code) {
  return [1, 2, 3, 4].includes(code);
}

function now() {
  return new Date().toLocaleTimeString();
}

function logEvent(name, data) {
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
      // Force canvas repaint when switching to RX (canvas sizes with CSS).
      if (target === "rx") redrawAll();
    });
  }
}

// ───────────────────────────────────────────────────── Received-file panel
function showCurrentFile(payload) {
  const info = document.getElementById("current-info");
  const wrap = document.getElementById("current-image-wrap");
  const mime = MIME_TYPES[payload.mime_type] || "application/octet-stream";
  info.innerHTML = `
    <div><strong>De :</strong> ${payload.callsign || "?"}</div>
    <div><strong>Nom :</strong> ${payload.filename}</div>
    <div><strong>Taille :</strong> ${payload.size} octets</div>
    <div><strong>MIME :</strong> ${mime}</div>
    <div><strong>σ² :</strong> ${payload.sigma2.toFixed(4)}</div>
    <div><strong>Chemin :</strong> <code>${payload.saved_path}</code></div>
  `;
  wrap.innerHTML = "";
  if (isImageMime(payload.mime_type)) {
    const { convertFileSrc } = window.__TAURI__.core;
    const img = document.createElement("img");
    img.src = convertFileSrc(payload.saved_path);
    img.alt = payload.filename;
    wrap.appendChild(img);
  }
}

// ───────────────────────────────────────────────────── Device / control
async function loadDevices() {
  const select = document.getElementById("device-select");
  const status = document.getElementById("status");
  if (!window.__TAURI__ || !window.__TAURI__.core) {
    status.textContent = "API Tauri indisponible";
    status.style.color = "#ef5350";
    return;
  }
  const { invoke } = window.__TAURI__.core;
  try {
    const devices = await invoke("list_audio_devices");
    select.innerHTML = "";
    if (devices.length === 0) {
      const opt = document.createElement("option");
      opt.textContent = "aucune carte son détectée";
      select.appendChild(opt);
      status.textContent = "aucune entrée";
      status.style.color = "#ef5350";
      return;
    }
    let preferred = null;
    for (const dev of devices) {
      const opt = document.createElement("option");
      opt.value = dev.name;
      const range = dev.max_sample_rate > 0
        ? `${dev.min_sample_rate}–${dev.max_sample_rate} Hz`
        : "?";
      const tag48 = dev.supports_48k ? " ✓48k" : "";
      const tagDef = dev.is_default ? " [default]" : "";
      const tagErr = dev.error ? ` [${dev.error}]` : "";
      opt.textContent = `${dev.friendly_name} — ${range}${tag48}${tagDef}${tagErr}`;
      opt.dataset.supports48k = dev.supports_48k;
      select.appendChild(opt);
      if (preferred === null && dev.supports_48k) preferred = opt;
      if (dev.is_default && dev.supports_48k) preferred = opt;
    }
    if (preferred) preferred.selected = true;
    const n48 = devices.filter(d => d.supports_48k).length;
    status.textContent = `${devices.length} entrée(s), ${n48} compat. 48 kHz`;
    document.getElementById("btn-start").disabled = n48 === 0;
  } catch (err) {
    status.textContent = `erreur : ${err}`;
    status.style.color = "#ef5350";
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
  const select = document.getElementById("device-select");
  const deviceName = select.value;
  const status = document.getElementById("status");
  levelCount = 0;
  try {
    await invoke("start_capture", { deviceName });
    status.textContent = "capture en cours";
    status.style.color = "#ffb74d";
    document.getElementById("btn-start").disabled = true;
    document.getElementById("btn-stop").disabled = false;
    select.disabled = true;
    logEvent("start", { device: deviceName });
  } catch (err) {
    status.textContent = `erreur start : ${err}`;
    status.style.color = "#ef5350";
    logEvent("error", { message: String(err) });
  }
}

async function stopCapture() {
  const { invoke } = window.__TAURI__.core;
  const status = document.getElementById("status");
  try {
    await invoke("stop_capture");
    status.textContent = "arrêté";
    status.style.color = "#9ccc65";
    document.getElementById("btn-start").disabled = false;
    document.getElementById("btn-stop").disabled = true;
    document.getElementById("device-select").disabled = false;
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
    } else {
      const path = await invoke("start_raw_recording");
      setRawButtonState(true);
      logEvent("raw_recording_started", { path });
    }
  } catch (err) {
    logEvent("raw_recording_error", { message: String(err) });
  }
}

let levelCount = 0;
function updateLevel(rms, peak, totalSamples) {
  levelCount += 1;
  const fill = document.getElementById("level-fill");
  const text = document.getElementById("level-text");
  const db = rms > 1e-6 ? 20 * Math.log10(rms) : -120;
  const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  fill.style.width = `${pct}%`;
  const samplesK = (totalSamples / 1000).toFixed(0);
  text.textContent = `${db.toFixed(1)} dB (peak ${peak.toFixed(2)}) #${levelCount} ${samplesK}k`;
}

function updateV2State(state) {
  const chip = document.getElementById("v2-state-chip");
  chip.className = `state-chip state-${state}`;
  chip.textContent = state.replace(/_/g, " ");
  if (state === "idle") {
    document.getElementById("v2-marker-info").textContent = "—";
    resetRxVisuals();
  }
}

function updateV2Marker(payload) {
  const info = document.getElementById("v2-marker-info");
  const kind = payload.is_meta ? "meta" : "data";
  info.textContent = `seg=${payload.seg_id} esi=${payload.base_esi} ${kind}`;
}

// ─────────────────────────────── Per-block progress + constellation state
let lastProgress = {
  bitmap: null,
  expected: 0,
  converged: 0,
  sigma2: null,
};
let lastConstellation = [];

function resetRxVisuals() {
  lastProgress = { bitmap: null, expected: 0, converged: 0, sigma2: null };
  lastConstellation = [];
  const text = document.getElementById("v2-progress-text");
  if (text) text.textContent = "—";
  const label = document.getElementById("progress-label");
  if (label) label.textContent = "—";
  const cinfo = document.getElementById("constellation-info");
  if (cinfo) cinfo.textContent = "—";
  drawProgressBlocks();
  drawConstellation();
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

  const sigmaStr = lastProgress.sigma2 != null ? lastProgress.sigma2.toFixed(3) : "?";
  const mini = document.getElementById("v2-progress-text");
  if (mini) mini.textContent = `${lastProgress.converged}/${lastProgress.expected} σ²=${sigmaStr}`;
  const label = document.getElementById("progress-label");
  if (label) {
    const pct = lastProgress.expected > 0
      ? (100 * lastProgress.converged / lastProgress.expected).toFixed(1)
      : "0.0";
    label.textContent = `${lastProgress.converged}/${lastProgress.expected} (${pct} %) — σ²=${sigmaStr}`;
  }
  const cinfo = document.getElementById("constellation-info");
  if (cinfo) cinfo.textContent = `${lastConstellation.length} symboles récents`;
  drawProgressBlocks();
  drawConstellation();
}

function redrawAll() {
  drawProgressBlocks();
  drawConstellation();
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
  const { bitmap, expected } = lastProgress;
  if (!expected || expected <= 0) {
    ctx.fillStyle = "#3a1a1a";
    ctx.fillRect(0, 0, w, h);
    return;
  }
  const bw = w / expected;
  for (let i = 0; i < expected; i++) {
    let converged = false;
    if (bitmap) {
      const byte = bitmap[i >> 3] || 0;
      converged = ((byte >> (i & 7)) & 1) !== 0;
    }
    ctx.fillStyle = converged ? "#9ccc65" : "#c62828";
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
      if (name === "file_complete") showCurrentFile(event.payload);
    });
  }
  listen("audio_level", (event) => {
    updateLevel(event.payload.rms, event.payload.peak, event.payload.total_samples);
  });

  listen("v2_state", (event) => {
    updateV2State(event.payload.state);
    logEvent("v2_state", event.payload);
  });
  listen("v2_marker", (event) => {
    updateV2Marker(event.payload);
    logEvent("v2_marker", event.payload);
  });
  listen("v2_signal_lost", () => logEvent("v2_signal_lost", null));
  listen("v2_signal_reacquired", () => logEvent("v2_signal_reacquired", null));
  listen("v2_session_end", (event) => logEvent("v2_session_end", event.payload));
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
}

async function init() {
  setupTabs();
  await loadDevices();
  await loadSaveDir();
  wireEvents();
  document.getElementById("btn-start").addEventListener("click", startCapture);
  document.getElementById("btn-stop").addEventListener("click", stopCapture);
  document.getElementById("btn-raw").addEventListener("click", toggleRawRecording);
  window.addEventListener("resize", redrawAll);
  await refreshRawRecordingState();
  resetRxVisuals();
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
