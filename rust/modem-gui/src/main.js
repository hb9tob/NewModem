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
    for (const dev of devices) {
      const opt = document.createElement("option");
      opt.value = dev.name;
      opt.textContent = `${dev.name} (${dev.default_sample_rate} Hz)${dev.is_default ? " [default]" : ""}`;
      if (dev.is_default) opt.selected = true;
      select.appendChild(opt);
    }
    status.textContent = `${devices.length} entrée(s)`;
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
