// Settings tab — device selects, PTT form, serial ports, save dir, modem-profile
// combos, experimental/force/turbo toggles, the settings-form save. loadSettings
// and setupLangSelect stay in main.js (cross-tab orchestration). Subscribes to
// sdr:persist-settings / sdr:reload-devices / capture:* on the bus; emits
// rx-device:changed for the Radio tab.
import { invoke } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings, modemProfiles, setModemProfiles } from "../lib/state.js";
import { logEvent } from "../lib/log.js";
import { on, emit } from "../lib/bus.js";
import { sdrBackends, isBackendEnabled, refreshSdrPanels, prefetchCapsForSelected } from "../lib/sdr.js";
import { now } from "../lib/format.js";
import { stopCapture } from "../lib/capture.js";

export function experimentalProfileNames() {
  return modemProfiles.filter((p) => p.experimental).map((p) => p.name);
}

export function refreshSettingsRxWarn() {
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

export function populateDeviceSelect(selectId, devices, savedName, backendDevices, direction) {
  const select = document.getElementById(selectId);
  if (!select) return null;
  select.innerHTML = "";
  const audio = devices || [];
  const sdrCount = (() => {
    let n = 0;
    for (const [, devs] of backendDevices || []) n += (devs || []).length;
    return n;
  })();
  if (audio.length === 0 && sdrCount === 0) {
    const opt = document.createElement("option");
    opt.textContent = t("status.no_device");
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
    opt.dataset.backend = "audio";
    select.appendChild(opt);
    if (preferred === null && dev.supports_48k) preferred = dev;
    if (dev.is_default && dev.supports_48k) preferred = dev;
  }
  for (const [backendId, devs] of backendDevices || []) {
    const info = sdrBackends.get(backendId);
    if (!info) continue;
    const supported = direction === "rx" ? info.capabilities.rx_supported : info.capabilities.tx_supported;
    if (!supported) continue;
    for (const dev of devs) {
      const opt = document.createElement("option");
      opt.value = dev.composite_name;
      opt.textContent = `${dev.friendly_name} ✓48k [SDR]`;
      opt.dataset.supports48k = "1";
      opt.dataset.backend = backendId;
      select.appendChild(opt);
    }
  }
  // Priority: saved value if still in the list, otherwise the preferred cpal entry.
  let savedFound = false;
  if (savedName) {
    for (let i = 0; i < select.options.length; i++) {
      if (select.options[i].value === savedName) { savedFound = true; break; }
    }
  }
  if (savedFound) {
    select.value = savedName;
  } else if (preferred) {
    select.value = preferred.name;
  }
  return select.value || null;
}

export async function loadDevices() {
  const status = document.getElementById("status");
  if (!window.__TAURI__ || !window.__TAURI__.core) {
    status.textContent = "API Tauri indisponible";
    status.style.color = "#ef5350";
    return;
  }
  try {
    // cpal soundcard lists in parallel; per-backend SDR device lists
    // come next (one Tauri call per registered backend, fanned out).
    const [rxDevices, txDevices] = await Promise.all([
      invoke("list_audio_devices"),
      invoke("list_output_audio_devices"),
    ]);
    const backendDevices = new Map();
    const backendTags = [];
    // Only enumerate backends the operator opted into via the
    // Paramètres checkboxes — keeps the GUI from triggering vendor-
    // library dlopens at startup on hosts that don't have the
    // hardware. The Tauri-side `list_sdr_devices` enforces the same
    // gate as defence-in-depth.
    await Promise.all(
      Array.from(sdrBackends.keys())
        .filter((id) => isBackendEnabled(id))
        .map(async (id) => {
          try {
            const list = await invoke("list_sdr_devices", { backendId: id });
            backendDevices.set(id, list);
            if (list.length > 0) {
              const info = sdrBackends.get(id);
              backendTags.push(` · ${list.length} ${info ? info.display_name : id}`);
            }
          } catch (err) {
            console.warn(`list_sdr_devices(${id}):`, err);
            backendDevices.set(id, []);
          }
        })
    );
    populateDeviceSelect(
      "rx-device-select", rxDevices, currentSettings.rx_device, backendDevices, "rx",
    );
    populateDeviceSelect(
      "tx-device-select", txDevices, currentSettings.tx_device, backendDevices, "tx",
    );
    const n48 = rxDevices.filter(d => d.supports_48k).length;
    status.textContent =
      `${rxDevices.length} RX (${n48} @48k) · ${txDevices.length} TX${backendTags.join("")}`;
    emit("rx:refresh-controls");
    // First render uses family caps (cache is empty); fetch the
    // per-device caps for whatever the persisted RX/TX devices are
    // and re-render once they land. Cheap when the selection is a
    // cpal soundcard — `prefetchCapsForSelected` no-ops for those.
    await Promise.all([
      prefetchCapsForSelected("rx-device-select"),
      prefetchCapsForSelected("tx-device-select"),
    ]);
    refreshSdrPanels();
    refreshTxHwVolume();
  } catch (err) {
    status.textContent = `erreur : ${err}`;
    status.style.color = "#ef5350";
  }
}

export function applyTurboModeStyling() {
  document.body.classList.toggle("turbo-mode", !!currentSettings.rx_turbo);
}

export function applyRxForceSettingsToUI() {
  const cb = document.getElementById("rx-force-mode");
  const sel = document.getElementById("rx-forced-profile");
  if (cb) cb.checked = !!currentSettings.rx_force_mode;
  if (sel) {
    sel.value = currentSettings.rx_forced_profile || "HIGH56";
    sel.disabled = !currentSettings.rx_force_mode;
  }
}

export async function loadModemProfiles() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    setModemProfiles(await invoke("list_modem_profiles"));
  } catch (err) {
    console.error("list_modem_profiles", err);
    setModemProfiles([]);
  }
  populateProfileSelects();
}

export function populateProfileSelects() {
  const allowExp = !!currentSettings.experimental_modes_enabled;
  populateOneProfileSelect("tx-mode", allowExp, /*rich=*/true);
  populateOneProfileSelect("rx-forced-profile", allowExp, /*rich=*/false);
}

export function populateOneProfileSelect(selId, allowExperimental, rich) {
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
        ? t("tx.experimental_label", { label: p.label })
        : p.label;
    } else {
      opt.textContent = p.experimental
        ? t("tx.experimental_name", { name: p.name })
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

export function applyExperimentalModesToUI() {
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

export function applyPttSettingsToUI() {
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

export async function loadSerialPorts() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
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
    opt.textContent = t("status.no_port_detected");
    sel.appendChild(opt);
  } else {
    if (saved && !ports.includes(saved)) {
      // Keep the saved value even when absent, to make it visible.
      const opt = document.createElement("option");
      opt.value = saved;
      opt.textContent = t("status.port_missing", { name: saved });
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

export function readPttFormIntoSettings() {
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

export function renderPttStatus(payload) {
  const el = document.getElementById("ptt-status");
  if (!el) return;
  const state = (payload && payload.state) || "off";
  const msg = (payload && payload.message) || "—";
  el.classList.remove("ok", "error", "off");
  el.classList.add(state);
  el.textContent = msg;
}

export async function persistSettings() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
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
  const saveWav = document.getElementById("tx-save-wav-enabled");
  if (saveWav) currentSettings.tx_save_wav = !!saveWav.checked;
  const deemph = document.getElementById("rx-deemphasis-enabled");
  if (deemph) currentSettings.rx_deemphasis_enabled = !!deemph.checked;
  const grid = document.getElementById("rx-allow-legacy-grid");
  if (grid) currentSettings.rx_allow_legacy_grid = !!grid.checked;
  const turbo = document.getElementById("rx-turbo-enabled");
  if (turbo) {
    currentSettings.rx_turbo = !!turbo.checked;
    applyTurboModeStyling();
  }
  const alsaBackend = document.getElementById("audio-backend-alsa");
  if (alsaBackend) currentSettings.audio_backend = alsaBackend.checked ? "alsa" : "cpal";

  // SDR-specific config is mutated directly on
  // `currentSettings.sdr_settings.backends[id].config` by the
  // `data-sdr-field` listeners (`onSdrFieldChange`) before
  // `persistSettings` runs — no per-backend reading block here.
  const statusEl = document.getElementById("settings-status");
  try {
    await invoke("save_settings", { settings: currentSettings });
    if (statusEl) statusEl.textContent = t("status.saved_at", { time: now() });
  } catch (err) {
    if (statusEl) statusEl.textContent = t("status.error_prefix", { err });
  }
}

export async function refreshTxHwVolume() {
  const row = document.getElementById("tx-hwvol-row");
  if (!row) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) { row.hidden = true; return; }
  const dev = ((currentSettings && currentSettings.tx_device) || "").trim();
  if (!dev) { row.hidden = true; return; }
  try {
    const hasMixer = await invoke("tx_device_has_mixer", { deviceName: dev });
    if (!hasMixer) { row.hidden = true; return; }
    const pct = await invoke("get_tx_volume", { deviceName: dev });
    if (pct == null) { row.hidden = true; return; }
    const slider = document.getElementById("tx-hwvol");
    const label = document.getElementById("tx-hwvol-val");
    if (slider) slider.value = String(pct);
    if (label) label.textContent = `${pct}%`;
    row.hidden = false;
  } catch (err) {
    console.warn("refreshTxHwVolume failed:", err);
    row.hidden = true;
  }
}

export function setupSettingsTab() {
  // Own the settings reactions to shared-bus events: lib/sdr.js asks for a
  // form save / device reload, and lib/capture.js signals RX up/down (the
  // RX-running warn-bar). Keeps these off main.js.
  on("sdr:persist-settings", () => persistSettings());
  on("sdr:reload-devices", () => loadDevices());
  on("capture:started", () => refreshSettingsRxWarn());
  on("capture:stopped", () => refreshSettingsRxWarn());
  const call = document.getElementById("callsign-input");
  const rxSel = document.getElementById("rx-device-select");
  const txSel = document.getElementById("tx-device-select");
  if (call) {
    const onCallsignChange = async () => {
      await persistSettings();
      emit("tx:refresh-estimate");
    };
    call.addEventListener("change", onCallsignChange);
    call.addEventListener("blur", onCallsignChange);
  }
  if (rxSel) {
    rxSel.addEventListener("change", async () => {
      emit("rx:refresh-controls");
      emit("rx-device:changed");
      // Re-render once with the (possibly stale) cached caps so the
      // panel updates immediately, then again once the per-device
      // caps come back over IPC. Backends without device-aware caps
      // get a single round-trip that returns family caps — same
      // visual result, slightly more invokes (acceptable: tens of µs).
      refreshSdrPanels();
      await prefetchCapsForSelected("rx-device-select");
      refreshSdrPanels();
      persistSettings();
    });
  }
  if (txSel) {
    txSel.addEventListener("change", async () => {
      refreshSdrPanels();
      await prefetchCapsForSelected("tx-device-select");
      refreshSdrPanels();
      persistSettings();
      refreshTxHwVolume();
    });
  }
  // Hardware playback-volume slider (Pi/ALSA only). Shown when the
  // selected TX device exposes a controllable mixer; drives the codec's
  // "Speaker" attenuator live (no settings round-trip — it's a hardware
  // mixer value, not a persisted app setting).
  const hwvol = document.getElementById("tx-hwvol");
  if (hwvol) {
    const label = document.getElementById("tx-hwvol-val");
    const paint = () => { if (label) label.textContent = `${hwvol.value}%`; };
    hwvol.addEventListener("input", paint);
    hwvol.addEventListener("change", async () => {
      paint();
      const dev = ((currentSettings && currentSettings.tx_device) || "").trim();
      if (!dev) return;
      try {
        await invoke("set_tx_volume", { deviceName: dev, pct: Number(hwvol.value) });
      } catch (err) {
        console.warn("set_tx_volume failed:", err);
      }
    });
  }
  // SDR-specific inputs are wired up at row-build time inside
  // `renderSdrPanel` (each `data-sdr-field` input gets an
  // `onSdrFieldChange` listener that mutates the per-backend config
  // and calls `persistSettings`). Nothing to register here.
  const preemph = document.getElementById("tx-preemphasis-enabled");
  if (preemph) preemph.addEventListener("change", persistSettings);
  const saveWav = document.getElementById("tx-save-wav-enabled");
  if (saveWav) saveWav.addEventListener("change", persistSettings);
  const deemph = document.getElementById("rx-deemphasis-enabled");
  if (deemph) deemph.addEventListener("change", persistSettings);
  // RX power-mode (legacy grid), turbo reception and audio backend had no
  // change listener, so toggling them did nothing until some OTHER control
  // happened to trigger persistSettings. Wire each so it saves immediately;
  // turbo also re-applies the dark-red theme on the spot.
  const gridCb = document.getElementById("rx-allow-legacy-grid");
  if (gridCb) gridCb.addEventListener("change", persistSettings);
  const turboCb = document.getElementById("rx-turbo-enabled");
  if (turboCb) {
    turboCb.addEventListener("change", () => {
      currentSettings.rx_turbo = turboCb.checked;
      applyTurboModeStyling();
      persistSettings();
    });
  }
  const alsaCb = document.getElementById("audio-backend-alsa");
  if (alsaCb) alsaCb.addEventListener("change", persistSettings);
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
  const fdxEnabled = document.getElementById("full-duplex-enabled");
  if (fdxEnabled) {
    fdxEnabled.addEventListener("change", () => {
      currentSettings.full_duplex_enabled = fdxEnabled.checked;
      // Re-evaluate the duplex bar visibility right away: turning the
      // toggle off mid-TX should hide the bar even before the next
      // tx_progress event arrives.
      emit("tx:refresh-duplex-bar");
      persistSettings();
    });
  }
}

export async function loadSaveDir() {
  try {
    const dir = await invoke("get_save_dir");
    document.getElementById("save-dir-label").textContent = `→ ${dir}`;
    document.getElementById("save-dir-label").title = dir;
  } catch (err) {
    console.error("get_save_dir", err);
  }
}
