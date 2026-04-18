// Phase 4b frontend : dropdown + start/stop + Tauri event log.

const MIME_TYPES = {
  0: "application/octet-stream",
  1: "image/avif",
  2: "image/jpeg",
  3: "image/png",
  4: "image/webp",
  5: "text/plain",
};

function mimeToExt(code, name) {
  const m = MIME_TYPES[code] || "application/octet-stream";
  return m;
}

function isImageMime(code) {
  return [1, 2, 3, 4].includes(code);
}

function now() {
  const d = new Date();
  return d.toLocaleTimeString();
}

function logEvent(name, data) {
  const log = document.getElementById("event-log");
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
  while (log.children.length > 200) log.removeChild(log.lastChild);
}

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
    logEvent("stop", null);
  } catch (err) {
    status.textContent = `erreur stop : ${err}`;
    status.style.color = "#ef5350";
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
}

async function init() {
  await loadDevices();
  await loadSaveDir();
  wireEvents();
  document.getElementById("btn-start").addEventListener("click", startCapture);
  document.getElementById("btn-stop").addEventListener("click", stopCapture);
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
