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
      select.appendChild(opt);
      if (preferred === null && dev.supports_48k) preferred = opt;
      if (dev.is_default && dev.supports_48k) preferred = opt;
    }
    if (preferred) preferred.selected = true;
    const n48 = devices.filter(d => d.supports_48k).length;
    status.textContent = `${devices.length} entrée(s), ${n48} compat. 48 kHz`;
  } catch (err) {
    status.textContent = `erreur : ${err}`;
    status.style.color = "#ef5350";
  }
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", loadDevices);
} else {
  loadDevices();
}
