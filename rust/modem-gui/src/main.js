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
      if (target === "rx") redrawAll();
      if (target === "sessions") refreshSessions();
      if (target === "channel") stopRxAndTxForChannelTab();
    });
  }
}

// Onglet Canal : on coupe RX et TX en cours en entrant. Le réglage
// d'atténuation s'applique au prochain TX, et un RX qui tourne pendant
// qu'on bidouille le slider risque de se faire saturer par notre propre
// signal de test plus tard (phase B).
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

// ─────────────────────────────────────────── Settings / device selection
// Les deux cartes son (RX/TX) + l'indicatif vivent dans l'onglet Paramètres
// et sont persistés via les commandes Tauri get_settings / save_settings.
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
};

function populateDeviceSelect(selectId, devices, savedName) {
  const select = document.getElementById(selectId);
  if (!select) return null;
  select.innerHTML = "";
  if (!devices || devices.length === 0) {
    const opt = document.createElement("option");
    opt.textContent = "aucune carte détectée";
    opt.value = "";
    select.appendChild(opt);
    return null;
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
    opt.dataset.supports48k = dev.supports_48k ? "1" : "0";
    select.appendChild(opt);
    if (preferred === null && dev.supports_48k) preferred = dev;
    if (dev.is_default && dev.supports_48k) preferred = dev;
  }
  // Priorité : valeur sauvegardée si encore disponible, sinon préférée.
  if (savedName && devices.some(d => d.name === savedName)) {
    select.value = savedName;
  } else if (preferred) {
    select.value = preferred.name;
  }
  return select.value || null;
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
  // Ne touche pas à btn-start si capture en cours (disabled via startCapture).
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
    const [rxDevices, txDevices] = await Promise.all([
      invoke("list_audio_devices"),
      invoke("list_output_audio_devices"),
    ]);
    populateDeviceSelect("rx-device-select", rxDevices, currentSettings.rx_device);
    populateDeviceSelect("tx-device-select", txDevices, currentSettings.tx_device);
    const n48 = rxDevices.filter(d => d.supports_48k).length;
    status.textContent = `${rxDevices.length} RX (${n48} @48k) · ${txDevices.length} TX`;
    refreshRxDeviceLabel();
    refreshStartButtonFromRx();
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
    };
  }
  const call = document.getElementById("callsign-input");
  if (call) call.value = currentSettings.callsign || "";
  applyPttSettingsToUI();
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
      // Conserve la valeur sauvegardée même absente, pour la rendre visible.
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
  currentSettings.callsign = (call && call.value || "").trim().toUpperCase();
  currentSettings.rx_device = rxSel ? rxSel.value || "" : "";
  currentSettings.tx_device = txSel ? txSel.value || "" : "";
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
      persistSettings();
    });
  }
  if (txSel) {
    txSel.addEventListener("change", persistSettings);
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
  levelCount = 0;
  try {
    await invoke("start_capture", { deviceName });
    status.textContent = "capture en cours";
    status.style.color = "#ffb74d";
    document.getElementById("btn-start").disabled = true;
    document.getElementById("btn-stop").disabled = false;
    if (select) select.disabled = true;
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
    document.getElementById("btn-stop").disabled = true;
    const rxSel = document.getElementById("rx-device-select");
    if (rxSel) rxSel.disabled = false;
    refreshStartButtonFromRx();
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
  hideFountainStatus();
  drawProgressBlocks();
  drawConstellation();
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
  // Ne cap pas le "reçu" à K — l'utilisateur a le droit de voir qu'il a
  // déjà avalé plus de blocs que le strict minimum (repair compris).
  // "Manquants" ne peut pas descendre en négatif : c'est max(0, K - R).
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
  const { bitmap, expected, converged } = lastProgress;
  if (!expected || expected <= 0) {
    ctx.fillStyle = "#3a1a1a";
    ctx.fillRect(0, 0, w, h);
    return;
  }
  // Stratégie "fountain fill" : le code RaptorQ n'a pas besoin de récupérer
  // les ESIs manquants exactement — il suffit de K blocs au total. On
  // affiche donc la bitmap réelle (positions ESI effectivement reçues),
  // puis on "bouche les trous" dès que `converged` dépasse le nombre de
  // bits à 1 dans [0..expected) : les ESIs > expected (venus par More ou
  // par repair) ne sont pas perdus, ils repeignent le premier trou rouge.
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
  // Surplus = blocs reçus au-delà de ce que la bitmap locale peut montrer.
  // Comble les trous de gauche à droite.
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
  resize: "800x600",
  freeW: 640,
  freeH: 480,
  // Défaut 1 volontairement : image horrible, force l'utilisateur à choisir.
  quality: 1,
  // Vitesse encodeur AVIF, 1..=10. 6 = équilibré (quelques secondes sur un
  // SP7), 1 = max compression/très lent, 10 = rapide mais fichier plus gros.
  speed: 6,
  // % de blocs repair RaptorQ ajoutés au burst initial (0, 5, 10, 20, 30,
  // 50, 100). 30 = défaut historique. 0 = strict K, utile quand on cherche
  // à tester la marge de décodage sans redondance.
  repairPct: 30,
  // Nombre de blocs à émettre en "More" burst (valeur exacte, pas un %).
  // L'user choisit depuis un select discret ou saisit via l'input libre.
  // L'user cas d'usage typique : "il me manque 5 blocs" → count = 5.
  moreCount: 5,
  aspectLinked: true,
  txActive: false,
  // Blocs fontaine additionnels à générer sur TX more (% de la taille code).
  morePct: 20,
  // État de la session TX en cours : conservé entre le TX initial et les
  // bursts "More" successifs pour pouvoir continuer l'ESI sans recouvrir
  // les packets déjà émis. Reset quand l'image ou le mode changent.
  lastTx: null,  // { esiMax, mode }
  compressedBytes: null,
  compressedUrl: null,
  compressing: false,
  compressTimer: null,
  compressSeq: 0,
  // True quand un paramètre (quality / resize / dimensions libres) a été
  // modifié depuis la dernière compression réussie. Pilote l'indicateur
  // "obsolète" + le style warn du bouton Recalculer.
  compressDirty: false,
  // Garde anti-réentrance : drop ignoré pendant qu'un chargement d'image
  // est en cours (évite deux loadTxFileFromPath en parallèle).
  loading: false,
  // Estimation calculée par le backend après chaque compression ou
  // changement de mode ; pilote l'activation du bouton TX et l'affichage
  // "durée estimée · nb blocs".
  estimate: null,
  // Suivi d'une émission en cours.
  progress: null,
  restartRxAfter: false,
};

// Chaîne de promesses pour sérialiser les compressions AVIF. Sans ça, un drop
// d'image pendant qu'une compression tourne lance un 2e encodeur ravif speed-1
// en parallèle — assez pour saturer la RAM et geler KDE sur les grosses images.
let _compressChain = Promise.resolve();

// Limites du transport (spec utilisateur) : interdit > 100 ko ou > 5 min,
// warning > 2 min. Sous ces seuils, TX est activé normalement.
const TX_HARD_BYTES = 100 * 1024;
const TX_HARD_SECONDS = 5 * 60;
const TX_WARN_SECONDS = 2 * 60;

function fmtSeconds(s) {
  if (!Number.isFinite(s)) return "—";
  const m = Math.floor(s / 60);
  const r = Math.round(s - m * 60);
  return `${m}:${String(r).padStart(2, "0")}`;
}

function refreshTxButtons() {
  const btnTx = document.getElementById("tx-btn-tx");
  const btnStop = document.getElementById("tx-btn-stop");
  const btnMore = document.getElementById("tx-btn-more");
  const btnCompress = document.getElementById("tx-btn-compress");
  const repairPct = document.getElementById("tx-repair-pct");
  const moreCount = document.getElementById("tx-more-count");
  const moreCountCustom = document.getElementById("tx-more-count-custom");
  if (!btnTx) return;
  const hasImage = !!txState.sourceImage;
  const hasCompressed = txState.compressedBytes != null;
  const est = txState.estimate;
  if (btnCompress) {
    btnCompress.disabled =
      !hasImage || txState.compressing || txState.txActive;
    if (txState.compressing) {
      btnCompress.textContent = "Compression…";
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

  // Validation stricte : interdire TX si payload > 100 ko ou durée > 5 min.
  const bytes = txState.compressedBytes || 0;
  const dur = est ? est.duration_s : 0;
  const tooBig = bytes > TX_HARD_BYTES;
  const tooLong = dur > TX_HARD_SECONDS;
  const warn = dur > TX_WARN_SECONDS && !tooLong;

  const canTx = hasImage
    && hasCompressed
    && !txState.compressing
    && !txState.txActive
    && !tooBig
    && !tooLong;
  btnTx.disabled = !canTx;
  // "More" disponible seulement après un TX initial réussi, et pas en TX actif.
  // Reset du lastTx si le mode a changé par rapport à celui du TX initial.
  const hasPriorTx =
    txState.lastTx != null && txState.lastTx.mode === txState.mode;
  btnMore.disabled = !hasImage || txState.txActive || !hasPriorTx;
  btnMore.title = moreButtonTitle();
  btnStop.disabled = !txState.txActive;
  if (repairPct) repairPct.disabled = !hasImage || txState.txActive;
  if (moreCount) moreCount.disabled = !hasImage || txState.txActive;
  if (moreCountCustom) {
    moreCountCustom.disabled = !hasImage || txState.txActive;
    moreCountCustom.hidden = moreCount ? moreCount.value !== "custom" : true;
  }

  // Libellé + couleur du bouton TX selon l'état.
  if (txState.txActive) {
    btnTx.textContent = "TX en cours…";
    btnTx.title = "émission en cours";
  } else if (tooBig) {
    btnTx.textContent = `TX ✖ fichier > 100 ko`;
    btnTx.title = `${(bytes / 1024).toFixed(1)} Kio dépasse la limite 100 Kio`;
  } else if (tooLong) {
    btnTx.textContent = `TX ✖ > 5 min`;
    btnTx.title = `durée estimée ${fmtSeconds(dur)} dépasse la limite 5 min`;
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

// Tooltip du bouton TX : durée, N émis, K requis, seuil K.
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

// Tooltip du bouton More : blocs additionnels, durée attendue.
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
  const base = txState.sourceFile.name.replace(/\.[^/.]+$/, "");
  // Envelope autorise 64 octets UTF-8, on garde un peu de marge.
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
  // Sérialise via _compressChain : on enchaîne la nouvelle compression après
  // celle qui est en cours, au lieu de laisser ravif tourner deux fois en
  // parallèle (cf. _compressChain supra).
  const chained = _compressChain
    .then(() => _runTxCompressImpl())
    .catch((err) => logEvent("tx_compress_chain_error", { message: String(err) }));
  _compressChain = chained;
  return chained;
}

async function _runTxCompressImpl() {
  if (!txState.sourceImage || !txState.sourceFile) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const { invoke, convertFileSrc } = window.__TAURI__.core;
  // Resync defensif depuis le DOM : en cas de drift entre clic radio et
  // cache JS (change event avalé, etc.), le DOM reste la source de vérité.
  const checkedRadio = document.querySelector('input[name="tx-resize"]:checked');
  if (checkedRadio && checkedRadio.value !== txState.resize) {
    txState.resize = checkedRadio.value;
  }
  const dims = txTargetDims();
  if (!dims) return;
  const seq = ++txState.compressSeq;
  txState.compressing = true;
  const previewEl = document.getElementById("tx-preview");
  if (previewEl) previewEl.classList.add("compressing");
  refreshTxPreview();
  refreshTxButtons();
  logEvent("tx_compress_start", {
    resize: txState.resize,
    target_w: dims.w,
    target_h: dims.h,
    quality: txState.quality,
    speed: txState.speed,
  });
  // Force le browser à peindre le loader avant de lancer invoke() : sans
  // ce yield, la compression backend peut répondre avant le premier paint,
  // la classe est retirée dans le finally, et l'utilisateur ne voit rien.
  await new Promise((r) =>
    requestAnimationFrame(() => requestAnimationFrame(r)),
  );
  try {
    const result = await invoke("compress_image", {
      opts: {
        target_w: dims.w,
        target_h: dims.h,
        quality: txState.quality,
        speed: txState.speed,
      },
    });
    if (seq !== txState.compressSeq) return; // stale
    txState.compressedBytes = result.byte_len;
    // Cache-bust: le fichier est réécrit à chaque appel.
    const url = `${convertFileSrc(result.preview_path)}?v=${Date.now()}`;
    txState.compressedUrl = url;
    // Les paramètres actuels correspondent à la nouvelle compression : on peut
    // effacer l'indicateur "obsolète".
    txState.compressDirty = false;
    const previewImg = document.getElementById("tx-preview-img");
    if (previewImg) previewImg.src = url;
    logEvent("tx_compress_done", {
      source_w: result.source_w,
      source_h: result.source_h,
      actual_w: result.actual_w,
      actual_h: result.actual_h,
      byte_len: result.byte_len,
    });
    // Rafraîchit l'estimation (durée + blocs) pour activer/verrouiller TX.
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

// Charge un fichier depuis un chemin disque (drag-drop natif Tauri). Le backend
// lit lui-même les bytes via set_tx_source_from_path : on évite complètement
// la sérialisation JSON-array via IPC qui, sur une grosse image, allouait
// ~10× la taille du fichier côté JS + côté Rust et pouvait faire geler KDE.
async function loadTxFileFromPath(path) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  // Anti-réentrance : ignore les drops successifs pendant qu'un chargement
  // ou une compression est en cours.
  if (txState.loading) {
    logEvent("tx_drop_ignored", { message: "chargement déjà en cours", path });
    return;
  }
  txState.loading = true;
  const { convertFileSrc, invoke } = window.__TAURI__.core;
  const url = convertFileSrc(path);
  const name = path.split(/[/\\]/).pop() || "image";
  try {
    // Upload par chemin (pas de bytes via IPC).
    const size = await invoke("set_tx_source_from_path", { path });
    // Charge aussi l'image en preview via asset://. L'ancien blob URL
    // (si issu d'un picker précédent) est libéré.
    if (txState.sourceUrl) {
      URL.revokeObjectURL(txState.sourceUrl);
      txState.sourceUrl = null;
    }
    const img = new Image();
    await new Promise((resolve, reject) => {
      img.onload = () => resolve();
      img.onerror = () => reject(new Error(`image load failed: ${path}`));
      img.src = url;
    });
    txState.sourceFile = { name, size };
    txState.sourceImage = img;
    txState.sourceSize = size;
    txState.compressedBytes = null;
    txState.compressedUrl = null;
    txState.compressDirty = false;
    txState.lastTx = null;
    if (txState.resize !== "free") {
      txState.freeW = img.naturalWidth;
      txState.freeH = img.naturalHeight;
      const fw = document.getElementById("tx-free-w");
      const fh = document.getElementById("tx-free-h");
      if (fw) fw.value = txState.freeW;
      if (fh) fh.value = txState.freeH;
    }
    document.getElementById("tx-drop-zone").hidden = true;
    const preview = document.getElementById("tx-preview");
    const previewImg = document.getElementById("tx-preview-img");
    if (previewImg) previewImg.src = url;
    if (preview) preview.hidden = false;
    refreshTxPreview();
    refreshTxButtons();
    scheduleTxCompress(50);
  } catch (err) {
    logEvent("tx_error", { message: `drop ${path}: ${err}` });
  } finally {
    txState.loading = false;
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
  txState.compressDirty = false;
  const url = URL.createObjectURL(file);
  txState.sourceUrl = url;
  const img = new Image();
  img.onload = async () => {
    txState.sourceImage = img;
    // Nouvelle image → nouvelle session (session_id RaptorQ dépend du contenu).
    txState.lastTx = null;
    // Ne reset freeW/H à la taille native QUE si on n'est pas déjà en mode
    // "libre" — sinon on écrase la dimension choisie par l'utilisateur
    // avant le drop, et la 1ère compression ignore son 800×600 manuel.
    if (txState.resize !== "free") {
      txState.freeW = img.naturalWidth;
      txState.freeH = img.naturalHeight;
      const fw = document.getElementById("tx-free-w");
      const fh = document.getElementById("tx-free-h");
      if (fw) fw.value = txState.freeW;
      if (fh) fh.value = txState.freeH;
    }
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
  txState.compressDirty = false;
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

  // Drag-drop : sur Linux/WebKitGTK les events HTML5 dragover/drop ne sont
  // pas remontés de façon fiable (le WM intercepte). On passe par les
  // events natifs Tauri v2 (dragDropEnabled:true dans tauri.conf.json),
  // émis au niveau fenêtre.
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
    // Nouveau mode → nouvelle session (session_id RaptorQ dépend du mode).
    txState.lastTx = null;
    refreshTxPreview();
    refreshTxEstimate();
    refreshTxButtons();
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

  const quality = document.getElementById("tx-quality");
  quality.addEventListener("input", () => {
    txState.quality = parseInt(quality.value, 10) || 0;
    document.getElementById("tx-quality-val").textContent = txState.quality;
    markCompressDirty();
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
        txState.repairPct = 30;
      }
      // Refresh estimate : la durée et N dépendent de ce %.
      refreshTxEstimate().catch(() => {});
      refreshTxButtons();
    });
  }

  const moreCountEl = document.getElementById("tx-more-count");
  const moreCountCustomEl = document.getElementById("tx-more-count-custom");
  if (moreCountEl) {
    moreCountEl.addEventListener("change", () => {
      if (moreCountEl.value === "custom") {
        if (moreCountCustomEl) {
          moreCountCustomEl.hidden = false;
          moreCountCustomEl.focus();
        }
      } else {
        if (moreCountCustomEl) moreCountCustomEl.hidden = true;
        const v = parseInt(moreCountEl.value, 10);
        if (Number.isFinite(v) && v > 0) txState.moreCount = v;
      }
      refreshTxButtons();
    });
  }
  if (moreCountCustomEl) {
    moreCountCustomEl.addEventListener("input", () => {
      const v = parseInt(moreCountCustomEl.value, 10);
      if (Number.isFinite(v) && v > 0) txState.moreCount = v;
      refreshTxButtons();
    });
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
    // Après un TX initial, on mémorise l'état session pour activer "More".
    // Le burst initial émet K + (repair_pct %) packets → ESI max = ceil(K * (1 + p/100)) - 1.
    const k = computeK();
    if (k) {
      const factor = 1 + (txState.repairPct || 0) / 100;
      const emitted = Math.ceil(k * factor);
      txState.lastTx = { mode: txState.mode, esiMax: emitted - 1 };
    }
  } catch (err) {
    logEvent("tx_start_error", { message: String(err) });
    txState.txActive = false;
    refreshTxButtons();
    await maybeRestartRx();
  }
}

// K RaptorQ = nombre de codewords source nécessaires au décodage.
// Fourni directement par le backend via l'estimate (k_source), ou approximé
// via total_blocks pour compatibilité avec un backend antérieur.
function computeK() {
  const est = txState.estimate;
  if (!est) return null;
  if (est.k_source != null) return Math.max(4, est.k_source);
  if (est.total_blocks != null) return Math.max(4, est.total_blocks);
  return null;
}

// Nombre de blocs additionnels à émettre en "More" burst. Soit la valeur
// discrète du select (1, 2, 5, 10, 20, 50), soit la valeur de l'input libre
// quand l'user a sélectionné "+n…".
function computeMoreCount() {
  const sel = document.getElementById("tx-more-count");
  if (!sel) return txState.moreCount || 5;
  if (sel.value === "custom") {
    const custom = document.getElementById("tx-more-count-custom");
    const v = parseInt(custom && custom.value, 10);
    return Number.isFinite(v) && v > 0 ? v : (txState.moreCount || 5);
  }
  const v = parseInt(sel.value, 10);
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
  // On retient où on va tomber après ce burst (count packets à partir d'esiStart).
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
  // Petit délai pour laisser la carte son TX libérer ses handles avant
  // d'ouvrir la capture RX (surtout si la même carte est utilisée).
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
      // K = blocs nécessaires au décodage (RaptorQ source), N = émis (K + repair).
      // Afficher les deux aide l'utilisateur à comprendre pourquoi la durée
      // dépasse le strict minimum et combien de marge le repair lui donne.
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
  // Réutilise la barre de progression du bas (blocs) en mode TX.
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

// ─────────────────────────────────────────── Onglet Canal (cascade ATT)
// Phase A : un seul réglage persistant (tx_attenuation_db dans Settings),
// alimenté soit à la main par le slider, soit par la médiane d'une liste
// de feedbacks reçus en QSO. Liste cascade : session JS uniquement.
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

async function init() {
  setupTabs();
  setupLightbox();
  setupTxTab();
  setupSettingsTab();
  await loadSettings();
  setupChannelTab();
  await loadDevices();
  await loadSerialPorts();
  await loadSaveDir();
  // Affiche l'état initial de la PTT (calculé par le backend au setup).
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
  // #HB9TOB: tick périodique pour effacer le chip OVD si aucun batch overdrive
  // n'est arrivé depuis OVD_STICKY_MS (utile aussi quand la capture est arrêtée).
  setInterval(refreshOverdriveChip, 200);
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
