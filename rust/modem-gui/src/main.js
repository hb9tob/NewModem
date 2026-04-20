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

// ─────────────────────────────────────────────────── Lightbox (double-clic)
// Affiche l'image plein écran avec zoom molette (jusqu'à 1:1 pixels natifs)
// et pan au drag. Échap ou clic hors image pour fermer.
const lightbox = {
  viewEl: null,
  imgEl: null,
  natW: 0,
  natH: 0,
  minScale: 1,
  scale: 1,
  tx: 0,
  ty: 0,
  dragging: false,
  lastX: 0,
  lastY: 0,
};

function openLightbox(src, alt) {
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
}

function closeLightbox() {
  if (!lightbox.viewEl) return;
  lightbox.viewEl.hidden = true;
  lightbox.imgEl.src = "";
}

function applyLightboxTransform() {
  const { imgEl, scale, tx, ty } = lightbox;
  if (!imgEl) return;
  imgEl.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
}

function fitLightbox() {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const fit = Math.min(vw / lightbox.natW, vh / lightbox.natH, 1);
  // min = fit-to-screen (mais plafonné à 1 si l'image est plus petite que
  // l'écran — pas de zoom out artificiel). max = 1 (1:1 pixel natif).
  lightbox.minScale = fit;
  lightbox.scale = fit;
  lightbox.tx = (vw - lightbox.natW * fit) / 2;
  lightbox.ty = (vh - lightbox.natH * fit) / 2;
  applyLightboxTransform();
}

function zoomLightbox(delta, cx, cy) {
  const prev = lightbox.scale;
  const factor = Math.exp(-delta * 0.0015);
  let next = prev * factor;
  next = Math.max(lightbox.minScale, Math.min(1, next));
  if (next === prev) return;
  // Zoom centré sur la position curseur : point-sous-curseur reste fixe.
  lightbox.tx = cx - (cx - lightbox.tx) * (next / prev);
  lightbox.ty = cy - (cy - lightbox.ty) * (next / prev);
  lightbox.scale = next;
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
  // Clic simple sur le fond (pas sur l'image) ferme. Double-clic ferme aussi.
  view.addEventListener("click", (ev) => {
    if (ev.target === view) closeLightbox();
  });
  view.addEventListener("dblclick", closeLightbox);
  window.addEventListener("keydown", (ev) => {
    if (view.hidden) return;
    if (ev.key === "Escape") closeLightbox();
  });
  window.addEventListener("resize", () => {
    if (!view.hidden) fitLightbox();
  });
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
  // Batch-on-buffer parallel decode: reuse showCurrentFile so the user sees
  // the high-quality batch result alongside the streaming-pipeline save.
  // The payload shape is compatible with showCurrentFile (callsign, filename,
  // size, mime_type, sigma2, saved_path) plus extra batch-specific fields.
  listen("batch_decode_complete", (event) => {
    const p = event.payload || {};
    logEvent("batch_decode_complete", {
      profile: p.profile,
      converged: `${p.converged_blocks}/${p.total_blocks}`,
      data_recovered: p.data_blocks_recovered,
      bytes: p.bytes_decoded,
      sigma2: p.sigma2,
      decode_ms: p.decode_ms,
      sample_count: p.sample_count,
    });
    if (p.saved_path) {
      showCurrentFile({
        callsign: p.callsign,
        filename: p.filename,
        size: p.bytes_decoded,
        mime_type: p.mime_type,
        sigma2: p.sigma2,
        saved_path: p.saved_path,
      });
    }
  });
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

// ────────────────────────────────────────────────────────────── TX tab (GUI)
// Le câblage backend (encodage AVIF, lancement TX, rendu audio) viendra
// ensuite. Ici on gère uniquement : chargement fichier (picker + DnD),
// dimensions cibles avec respect de l'aspect, état des contrôles.
const txState = {
  sourceFile: null,
  sourceImage: null,
  sourceSize: 0,
  sourceUrl: null,
  mode: "HIGH",
  resize: "none",
  freeW: 640,
  freeH: 480,
  // Défaut 1 volontairement : image horrible, force l'utilisateur à choisir.
  quality: 1,
  aspectLinked: true,
  txActive: false,
  // Blocs fontaine additionnels à générer sur TX more (% de la taille code).
  morePct: 10,
  compressedBytes: null,
  compressedUrl: null,
  compressing: false,
  compressTimer: null,
  compressSeq: 0,
};

function refreshTxButtons() {
  const btnTx = document.getElementById("tx-btn-tx");
  const btnStop = document.getElementById("tx-btn-stop");
  const btnMore = document.getElementById("tx-btn-more");
  const morePct = document.getElementById("tx-more-pct");
  if (!btnTx) return;
  const hasImage = !!txState.sourceImage;
  btnTx.disabled = !hasImage || txState.txActive;
  btnMore.disabled = !hasImage || txState.txActive;
  btnStop.disabled = !txState.txActive;
  if (morePct) morePct.disabled = !hasImage || txState.txActive;
}

function txFormatBytes(n) {
  if (n == null) return "—";
  if (n < 1024) return `${n} o`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} Kio`;
  return `${(n / 1024 / 1024).toFixed(2)} Mio`;
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
  if (!txState.sourceImage) {
    if (info) info.textContent = "—";
    if (srcSize) srcSize.textContent = "—";
    if (cmpSize) cmpSize.textContent = "—";
    return;
  }
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
  if (srcSize) srcSize.textContent = txFormatBytes(txState.sourceSize);
  if (cmpSize) {
    if (txState.compressing && txState.compressedBytes == null) {
      cmpSize.textContent = "compression…";
    } else if (txState.compressedBytes != null) {
      const ratio = txState.sourceSize > 0
        ? ` (${(txState.compressedBytes / txState.sourceSize * 100).toFixed(1)}%)`
        : "";
      cmpSize.textContent = `${txFormatBytes(txState.compressedBytes)}${ratio}`;
    } else {
      cmpSize.textContent = "—";
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

async function runTxCompress() {
  if (!txState.sourceImage || !txState.sourceFile) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke, convertFileSrc } = window.__TAURI__.core;
  const dims = txTargetDims();
  if (!dims) return;
  const seq = ++txState.compressSeq;
  txState.compressing = true;
  refreshTxPreview();
  try {
    const result = await invoke("compress_image", {
      opts: {
        target_w: dims.w,
        target_h: dims.h,
        quality: txState.quality,
      },
    });
    if (seq !== txState.compressSeq) return; // stale
    txState.compressedBytes = result.byte_len;
    // Cache-bust: le fichier est réécrit à chaque appel.
    const url = `${convertFileSrc(result.preview_path)}?v=${Date.now()}`;
    txState.compressedUrl = url;
    const previewImg = document.getElementById("tx-preview-img");
    if (previewImg) previewImg.src = url;
  } catch (err) {
    if (seq === txState.compressSeq) {
      logEvent("tx_compress_error", { message: String(err) });
    }
  } finally {
    if (seq === txState.compressSeq) {
      txState.compressing = false;
      refreshTxPreview();
      refreshTxButtons();
    }
  }
}

async function loadTxFile(file) {
  if (!file || !file.type || !file.type.startsWith("image/")) {
    logEvent("tx_error", { message: `type non supporté: ${file && file.type}` });
    return;
  }
  // Libère l'ancien blob URL s'il existe.
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  txState.sourceFile = file;
  txState.sourceSize = file.size;
  txState.compressedBytes = null;
  txState.compressedUrl = null;
  const url = URL.createObjectURL(file);
  txState.sourceUrl = url;
  const img = new Image();
  img.onload = async () => {
    txState.sourceImage = img;
    // Init des champs "libre" à la taille native.
    txState.freeW = img.naturalWidth;
    txState.freeH = img.naturalHeight;
    const fw = document.getElementById("tx-free-w");
    const fh = document.getElementById("tx-free-h");
    if (fw) fw.value = txState.freeW;
    if (fh) fh.value = txState.freeH;
    document.getElementById("tx-drop-zone").hidden = true;
    const preview = document.getElementById("tx-preview");
    const previewImg = document.getElementById("tx-preview-img");
    previewImg.src = url;
    preview.hidden = false;
    refreshTxPreview();
    refreshTxButtons();
    // Envoie la source au backend pour compressions successives sans re-upload.
    try {
      const buf = await file.arrayBuffer();
      const { invoke } = window.__TAURI__.core;
      await invoke("set_tx_source", { bytes: Array.from(new Uint8Array(buf)) });
      scheduleTxCompress(50);
    } catch (err) {
      logEvent("tx_error", { message: `upload source: ${err}` });
    }
  };
  img.onerror = () => {
    logEvent("tx_error", { message: `impossible de charger ${file.name}` });
  };
  img.src = url;
}

async function resetTxFile() {
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  txState.sourceFile = null;
  txState.sourceImage = null;
  txState.sourceSize = 0;
  txState.compressedBytes = null;
  txState.compressedUrl = null;
  txState.compressSeq++;
  if (txState.compressTimer) {
    clearTimeout(txState.compressTimer);
    txState.compressTimer = null;
  }
  const drop = document.getElementById("tx-drop-zone");
  const preview = document.getElementById("tx-preview");
  const previewImg = document.getElementById("tx-preview-img");
  const fileInput = document.getElementById("tx-file-input");
  if (preview) preview.hidden = true;
  if (drop) drop.hidden = false;
  if (previewImg) previewImg.src = "";
  if (fileInput) fileInput.value = "";
  refreshTxPreview();
  refreshTxButtons();
  try {
    const { invoke } = window.__TAURI__.core;
    await invoke("clear_tx_source");
  } catch {
    // peu importe : le state JS est déjà réinitialisé.
  }
}

function setupTxTab() {
  const drop = document.getElementById("tx-drop-zone");
  const fileInput = document.getElementById("tx-file-input");
  if (!drop || !fileInput) return;

  drop.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    if (fileInput.files && fileInput.files[0]) loadTxFile(fileInput.files[0]);
  });

  // Drag-drop sur l'ensemble de la zone image (drop-zone OU preview), de
  // façon à pouvoir remplacer l'image en la glissant dessus.
  const area = document.querySelector(".tx-image-area");
  const onDragOver = (ev) => {
    ev.preventDefault();
    if (ev.dataTransfer) ev.dataTransfer.dropEffect = "copy";
    drop.classList.add("drag-over");
  };
  const onDragLeave = () => drop.classList.remove("drag-over");
  const onDrop = (ev) => {
    ev.preventDefault();
    drop.classList.remove("drag-over");
    const file = ev.dataTransfer && ev.dataTransfer.files && ev.dataTransfer.files[0];
    if (file) loadTxFile(file);
  };
  area.addEventListener("dragover", onDragOver);
  area.addEventListener("dragleave", onDragLeave);
  area.addEventListener("drop", onDrop);

  document.getElementById("tx-preview-reset").addEventListener("click", (ev) => {
    ev.stopPropagation();
    resetTxFile();
  });

  document.getElementById("tx-mode").addEventListener("change", (ev) => {
    txState.mode = ev.target.value;
    refreshTxPreview();
  });

  const resizeRadios = document.querySelectorAll('input[name="tx-resize"]');
  for (const r of resizeRadios) {
    r.addEventListener("change", () => {
      if (!r.checked) return;
      txState.resize = r.value;
      document.getElementById("tx-resize-free").hidden = r.value !== "free";
      refreshTxPreview();
      scheduleTxCompress();
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
    refreshTxPreview();
    if (txState.resize === "free") scheduleTxCompress();
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
    refreshTxPreview();
    if (txState.resize === "free") scheduleTxCompress();
  });

  const quality = document.getElementById("tx-quality");
  quality.addEventListener("input", () => {
    txState.quality = parseInt(quality.value, 10) || 0;
    document.getElementById("tx-quality-val").textContent = txState.quality;
    refreshTxPreview();
    scheduleTxCompress();
  });

  // Boutons TX / Stop / TX more — handlers placeholder jusqu'au câblage
  // backend. Les transitions d'état (txActive on/off) seront pilotées par
  // les événements Tauri une fois la pipeline TX branchée.
  document.getElementById("tx-btn-tx").addEventListener("click", () => {
    logEvent("tx_click", {
      action: "tx",
      mode: txState.mode,
      resize: txState.resize,
      dims: txTargetDims(),
      quality: txState.quality,
      file: txState.sourceFile && txState.sourceFile.name,
      note: "backend TX à câbler",
    });
  });
  document.getElementById("tx-btn-stop").addEventListener("click", () => {
    logEvent("tx_click", { action: "stop", note: "backend TX à câbler" });
  });
  document.getElementById("tx-btn-more").addEventListener("click", () => {
    logEvent("tx_click", {
      action: "tx_more",
      mode: txState.mode,
      resize: txState.resize,
      dims: txTargetDims(),
      quality: txState.quality,
      more_pct: txState.morePct,
      file: txState.sourceFile && txState.sourceFile.name,
      note: "backend TX à câbler",
    });
  });
  document.getElementById("tx-more-pct").addEventListener("change", (ev) => {
    txState.morePct = parseInt(ev.target.value, 10) || 10;
  });
  refreshTxButtons();
}

async function init() {
  setupTabs();
  setupLightbox();
  setupTxTab();
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
