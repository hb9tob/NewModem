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

// #HB9TOB: durée d'affichage rouge du chip OVD après dernière détection.
// Le chip s'efface tout seul après ce délai si plus aucun batch n'est marqué
// overdrive. Voir OVERDRIVE_* côté Rust pour le seuil de détection.
const OVD_STICKY_MS = 5000;

// #HB9TOB: PAPR de référence audio-passband par profil (peak/RMS, dB), à
// calibrer empiriquement sur des captures clean ; affiché à côté du chip OVD
// pour comparer la mesure courante. La détection d'overdrive utilise un seuil
// unique côté Rust (OVERDRIVE_CREST_GATE_DB), pas cette table.
//   - HIGH/MEGA (16-APSK, 2 anneaux) : calibré 2026-04-19 sur OTA MEGA
//     (capture-1776547952 etc.) p50 ≈ 9.48 dB
//   - ULTRA/ROBUST/NORMAL : valeurs provisoires à confirmer sur capture clean
//     du profil correspondant
const PAPR_REF_DB = {
  ULTRA: 8.5,
  ROBUST: 8.5,
  NORMAL: 8.5,
  HIGH: 9.5,
  MEGA: 9.5,
};

let lastOverdriveMs = 0;
let lastCrestDb = NaN;
let currentProfile = null;

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

function refreshPaprInfo() {
  const elt = document.getElementById("papr-info");
  if (!elt) return;
  const ref = currentProfile && PAPR_REF_DB[currentProfile] != null
    ? PAPR_REF_DB[currentProfile]
    : null;
  const measuredStr = Number.isFinite(lastCrestDb) && lastCrestDb > 0
    ? `${lastCrestDb.toFixed(1)}` : "—";
  const refStr = ref != null ? `${ref.toFixed(1)}` : "—";
  const profileTag = currentProfile ? ` ${currentProfile}` : "";
  elt.textContent = `PAPR ${measuredStr} / réf ${refStr} dB${profileTag}`;
  // Souligne en orange si on est franchement sous la référence (≥ 1 dB de
  // compression). Indication visuelle complémentaire au chip rouge.
  const warn = ref != null
    && Number.isFinite(lastCrestDb)
    && lastCrestDb > 0
    && lastCrestDb < ref - 1.0;
  elt.classList.toggle("papr-warn", warn);
}

function noteAudioOverdrive(overdrive, crestDb) {
  if (Number.isFinite(crestDb)) lastCrestDb = crestDb;
  if (overdrive) lastOverdriveMs = Date.now();
  refreshOverdriveChip();
  refreshPaprInfo();
}

function noteProfileFromHeader(profileStr) {
  currentProfile = (profileStr || "").toUpperCase() || null;
  refreshPaprInfo();
}

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
  // #HB9TOB: tick périodique pour effacer le chip OVD si aucun batch overdrive
  // n'est arrivé depuis OVD_STICKY_MS (utile aussi quand la capture est arrêtée).
  setInterval(refreshOverdriveChip, 200);
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
