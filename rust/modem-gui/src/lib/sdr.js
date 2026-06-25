// Shared layer — the SDR subsystem: device-capability cache, per-backend
// config, the Settings/Radio control-row builders, and the frequency MRU.
// Settings AND Radio both drive it; upward effects (persist settings, reload
// the device list, refresh the Radio gain panel) are emitted on the bus so this
// module never imports a tab.
import { invoke, convertFileSrc } from "./ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "./state.js";
import { getSelectedBackendId, makeRow, makeFieldLabel, rxIsRunning } from "./dom.js";
import { escapeHtml } from "./format.js";
import { emit } from "./bus.js";

export let sdrBackends = new Map();

export const EIA_CTCSS_TONES_HZ = [
  67.0, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8,
  97.4, 100.0, 103.5, 107.2, 110.9, 114.8, 118.8, 123.0, 127.3, 131.8,
  136.5, 141.3, 146.2, 151.4, 156.7, 162.2, 167.9, 173.8, 179.9, 186.2,
  192.8, 203.5, 210.7, 218.1, 225.7, 233.6, 241.8, 250.3, 254.1,
];

export async function loadSdrBackends() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    const backends = await invoke("list_sdr_backends");
    sdrBackends = new Map(backends.map(b => [b.id, b]));
  } catch (err) {
    console.error("list_sdr_backends:", err);
    sdrBackends = new Map();
  }
}

export function isBackendEnabled(backendId) {
  if (!currentSettings.sdr_settings) return false;
  const entry = currentSettings.sdr_settings.backends &&
    currentSettings.sdr_settings.backends[backendId];
  return entry ? entry.enabled === true : false;
}

export async function renderSdrBackendsList() {
  const host = document.getElementById("sdr-backends-list");
  if (!host) return;
  if (!sdrBackends || sdrBackends.size === 0) {
    host.innerHTML = `<span class="tx-hint">${escapeHtml(t("status.no_sdr_backends"))}</span>`;
    return;
  }
  host.innerHTML = "";
  for (const [id, info] of sdrBackends.entries()) {
    // Seed the settings entry so the persisted bool round-trips
    // even when the user hasn't touched the checkbox yet.
    ensureBackendEntry(id);
    const label = document.createElement("label");
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.dataset.backendId = id;
    cb.checked = isBackendEnabled(id);
    const name = document.createElement("span");
    name.textContent = t("status.sdr_enable", { name: info.display_name || id });
    const status = document.createElement("span");
    status.className = "sdr-backend-status";
    status.dataset.backendId = id;
    status.textContent = "";
    label.appendChild(cb);
    label.appendChild(name);
    label.appendChild(status);
    host.appendChild(label);

    cb.addEventListener("change", async () => {
      const entry = ensureBackendEntry(id);
      entry.enabled = cb.checked;
      try {
        await invoke("save_settings", { settings: currentSettings });
      } catch (err) {
        console.error("save_settings (backend enable):", err);
      }
      // Refresh both the inline status (re-attempts the dlopen when
      // turning on) and the device dropdown.
      await refreshBackendLibraryStatus(id);
      await emit("sdr:reload-devices");
    });
  }
  // Initial status sweep — only ping enabled backends so we don't
  // dlopen vendor libraries just to render the panel.
  for (const id of sdrBackends.keys()) {
    if (isBackendEnabled(id)) await refreshBackendLibraryStatus(id);
  }
}

export async function refreshBackendLibraryStatus(backendId) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const span = document.querySelector(
    `.sdr-backend-status[data-backend-id="${backendId}"]`
  );
  if (!span) return;
  if (!isBackendEnabled(backendId)) {
    span.textContent = "";
    span.classList.remove("warn");
    return;
  }
  try {
    const st = await invoke("get_backend_library_status", { backendId });
    if (st.available) {
      span.textContent = t("status.sdr_lib_ok");
      span.classList.remove("warn");
    } else {
      span.textContent = st.message || t("status.lib_missing");
      span.classList.add("warn");
    }
  } catch (err) {
    span.textContent = t("status.lib_status_unavail");
    span.classList.add("warn");
  }
}

export const deviceCapsCache = new Map();

export let pendingCapsFetch = new Map();   // composite_name → Promise

export async function resolveDeviceCaps(compositeName, backendId) {
  if (!compositeName) return null;
  if (deviceCapsCache.has(compositeName)) return deviceCapsCache.get(compositeName);
  if (pendingCapsFetch.has(compositeName)) return pendingCapsFetch.get(compositeName);
  if (!window.__TAURI__ || !window.__TAURI__.core) return null;
  const promise = (async () => {
    try {
      const caps = await invoke("get_sdr_device_capabilities", { compositeName });
      deviceCapsCache.set(compositeName, caps);
      return caps;
    } catch (err) {
      console.error("get_sdr_device_capabilities:", err);
      const family = sdrBackends.get(backendId);
      const fallback = family ? family.capabilities : null;
      if (fallback) deviceCapsCache.set(compositeName, fallback);
      return fallback;
    } finally {
      pendingCapsFetch.delete(compositeName);
    }
  })();
  pendingCapsFetch.set(compositeName, promise);
  return promise;
}

export async function prefetchCapsForSelected(selectId) {
  const sel = document.getElementById(selectId);
  if (!sel || !sel.options[sel.selectedIndex]) return;
  const opt = sel.options[sel.selectedIndex];
  const backendId = opt.dataset && opt.dataset.backend;
  if (!backendId || backendId === "audio") return;
  await resolveDeviceCaps(sel.value, backendId);
}

export function getCapsForSelected(selectId) {
  const sel = document.getElementById(selectId);
  if (!sel || !sel.options[sel.selectedIndex]) return null;
  const opt = sel.options[sel.selectedIndex];
  const backendId = opt.dataset && opt.dataset.backend;
  if (!backendId || backendId === "audio") return null;
  const composite = sel.value;
  const cached = composite ? deviceCapsCache.get(composite) : null;
  if (cached) return cached;
  const family = sdrBackends.get(backendId);
  return family ? family.capabilities : null;
}

export function ensureBackendConfig(backendId) {
  if (!currentSettings.sdr_settings) currentSettings.sdr_settings = { backends: {} };
  if (!currentSettings.sdr_settings.backends) currentSettings.sdr_settings.backends = {};
  let entry = currentSettings.sdr_settings.backends[backendId];
  if (!entry) {
    entry = { config: makeDefaultSdrConfig(backendId), freq_favorites: [] };
    currentSettings.sdr_settings.backends[backendId] = entry;
  }
  if (!entry.config) entry.config = makeDefaultSdrConfig(backendId);
  if (!entry.freq_favorites) entry.freq_favorites = [];
  return entry.config;
}

export function ensureBackendEntry(backendId) {
  ensureBackendConfig(backendId);
  return currentSettings.sdr_settings.backends[backendId];
}

export function makeDefaultSdrConfig(backendId) {
  if (backendId === "pluto") {
    return {
      backend_id: "pluto", device_id: "",
      rx_freq_hz: 145_500_000, tx_freq_hz: 145_500_000,
      gain: { kind: "agc_mode", id: "slow_attack" },
      max_deviation_hz: 5000.0, tx_deviation_hz: 5000.0,
      antenna: "",
      bias_t: false, fm_notch: false, dab_notch: false,
      ctcss_freq_hz: 0.0, ctcss_level: 0.1,
      rf_bandwidth_hz: 200_000,
      backend_extras: { tx_attenuation_db: 30.0, prefer_low_rate: true },
    };
  }
  if (backendId === "sdrplay") {
    return {
      backend_id: "sdrplay", device_id: "",
      rx_freq_hz: 145_500_000, tx_freq_hz: 145_500_000,
      gain: { kind: "manual", shape: "lna_plus_if", lna_state: 4, if_grdb: 40 },
      max_deviation_hz: 5000.0, tx_deviation_hz: 5000.0,
      antenna: "fifty",
      bias_t: false, fm_notch: false, dab_notch: false,
      ctcss_freq_hz: 0.0, ctcss_level: 0.1,
      rf_bandwidth_hz: null,
      backend_extras: { tuner: "B", decimation: 4 },
    };
  }
  if (backendId === "rtlsdr") {
    // Mirrors `default_sdr_config_for("rtlsdr")` in settings.rs.
    // Step 22 ≈ 40.2 dB on the R820T-family ladder — enough head-room
    // for typical 2 m signals out of the box; the operator drops it
    // via the gain dropdown when receiving a strong neighbour.
    return {
      backend_id: "rtlsdr", device_id: "",
      rx_freq_hz: 145_500_000, tx_freq_hz: 145_500_000,
      gain: { kind: "manual", shape: "discrete", step_idx: 22 },
      max_deviation_hz: 5000.0, tx_deviation_hz: 5000.0,
      antenna: "",
      bias_t: false, fm_notch: false, dab_notch: false,
      ctcss_freq_hz: 0.0, ctcss_level: 0.1,
      rf_bandwidth_hz: null,
      backend_extras: { ppm_correction: 0, direct_sampling: false },
    };
  }
  return {
    backend_id: backendId, device_id: "",
    rx_freq_hz: 0, tx_freq_hz: 0,
    gain: { kind: "manual", shape: "db", db: 0 },
    max_deviation_hz: 5000.0, tx_deviation_hz: 5000.0,
    antenna: "",
    bias_t: false, fm_notch: false, dab_notch: false,
    ctcss_freq_hz: 0.0, ctcss_level: 0.1,
    rf_bandwidth_hz: null,
    backend_extras: {},
  };
}

export function renderSdrPanel(direction) {
  const panel = document.getElementById(`sdr-${direction}-panel`);
  const rowsEl = document.getElementById(`sdr-${direction}-rows`);
  const hintEl = document.getElementById(`sdr-${direction}-hint`);
  if (!panel || !rowsEl) return;
  const backendId = getSelectedBackendId(`${direction}-device-select`);
  const info = backendId ? sdrBackends.get(backendId) : null;
  if (!info) {
    panel.hidden = true;
    rowsEl.innerHTML = "";
    if (hintEl) hintEl.textContent = "";
    return;
  }
  // Prefer per-device caps when the backend has shipped them
  // (SDRplay RSP1A vs RSP1 vs RSPduo render very different panels);
  // fall back to family caps before the first device pick.
  const caps = getCapsForSelected(`${direction}-device-select`) || info.capabilities;
  const supported = direction === "rx" ? caps.rx_supported : caps.tx_supported;
  if (!supported) {
    panel.hidden = true;
    rowsEl.innerHTML = "";
    if (hintEl) hintEl.textContent = "";
    return;
  }
  // RX SDR parameters now live entirely on the Radio tab: tuning + the
  // gain/AGC bar (renderRadioGain) + the "⚙ Réglages SDR" popover
  // (renderRadioSdrParams). Keep the Settings RX device fieldset free of
  // duplicate (and potentially conflicting) controls — show only a pointer.
  if (direction === "rx") {
    panel.hidden = false;
    panel.dataset.backend = backendId;
    rowsEl.innerHTML = "";
    if (hintEl) hintEl.textContent = t("settings.rx_sdr_in_radio_tab");
    return;
  }
  panel.hidden = false;
  panel.dataset.backend = backendId;
  const cfg = ensureBackendConfig(backendId);
  rowsEl.innerHTML = "";
  rowsEl.appendChild(buildFreqRow(direction, backendId, caps, cfg));
  if (caps.agc_modes && caps.agc_modes.length > 0) {
    rowsEl.appendChild(buildAgcRow(direction, backendId, caps, cfg));
  }
  rowsEl.appendChild(buildGainRow(direction, backendId, caps, cfg));
  if (direction === "rx" && caps.antennas && caps.antennas.length > 0) {
    rowsEl.appendChild(buildAntennaRow(backendId, caps, cfg));
  }
  if (hasFeatureToggles(caps)) {
    rowsEl.appendChild(buildFeatureRow(direction, backendId, caps, cfg));
  }
  rowsEl.appendChild(buildDeviationRow(direction, backendId, cfg));
  if (direction === "tx" && caps.features.ctcss_tx) {
    rowsEl.appendChild(buildCtcssRow(backendId, cfg));
  }
  if (direction === "tx") {
    rowsEl.appendChild(buildTxAttenuationRow(backendId, cfg));
  }
  rowsEl.appendChild(buildBackendExtrasRow(direction, backendId, caps, cfg));
  if (hintEl) hintEl.textContent = "";
}

export function refreshSdrPanels() {
  renderSdrPanel("rx");
  renderSdrPanel("tx");
  // Keep the Radio-tab gain/AGC controls and the SDR popover in sync (e.g.
  // an AGC change re-renders to enable/disable the gain inputs).
  emit("sdr:refresh-radio-gain");
  const modal = document.getElementById("radio-sdr-modal");
  if (modal && !modal.hidden) emit("sdr:refresh-radio-sdr-params");
}

export function hasFeatureToggles(caps) {
  const f = caps.features;
  return !!(f && (f.bias_t || f.fm_notch || f.dab_notch));
}

export function buildFreqRow(direction, backendId, caps, cfg) {
  const row = makeRow();
  const range = direction === "rx" ? caps.rx_freq_range_hz : caps.tx_freq_range_hz;
  const minMhz = range ? (range[0] / 1e6) : 0.001;
  const maxMhz = range ? (range[1] / 1e6) : 6000;
  const label = document.createElement("label");
  label.className = "pluto-field";
  label.textContent = t(direction === "rx" ? "rx.dev_freq_rx" : "rx.dev_freq_tx");
  const input = document.createElement("input");
  input.type = "number";
  input.id = `sdr-${direction}-freq-${backendId}`;
  input.step = "0.001";
  input.min = String(minMhz);
  input.max = String(maxMhz);
  const fieldHz = direction === "rx" ? "rx_freq_hz" : "tx_freq_hz";
  if (Number.isFinite(cfg[fieldHz]) && cfg[fieldHz] > 0) {
    input.value = (cfg[fieldHz] / 1e6).toFixed(3);
  }
  input.dataset.sdrField = fieldHz;
  input.dataset.sdrTransform = "mhz_to_hz";
  input.dataset.backend = backendId;
  input.addEventListener("change", onSdrFieldChange);
  label.appendChild(input);
  row.appendChild(label);
  return row;
}

export function buildAgcRow(direction, backendId, caps, cfg) {
  const row = makeRow();
  const label = document.createElement("label");
  label.className = "pluto-field";
  label.textContent = t("rx.dev_agc");
  const sel = document.createElement("select");
  sel.id = `sdr-${direction}-agc-${backendId}`;
  sel.dataset.backend = backendId;
  for (const mode of caps.agc_modes) {
    const opt = document.createElement("option");
    opt.value = mode.id;
    opt.textContent = mode.label;
    opt.dataset.manual = mode.manual ? "1" : "0";
    // Per-mode "AGC keeps LNA manual" hint — SDRplay's AGC loop
    // only manages IF gRdB. Stashed on the option so buildGainRow
    // can read it back without re-walking caps.
    opt.dataset.keepsLna = mode.keeps_lna_manual ? "1" : "0";
    sel.appendChild(opt);
  }
  const gain = cfg.gain || {};
  if (gain.kind === "agc_mode" && gain.id) {
    sel.value = gain.id;
  } else if (gain.kind === "manual") {
    const m = caps.agc_modes.find(x => x.manual);
    if (m) sel.value = m.id;
  }
  sel.addEventListener("change", () => {
    const opt = sel.options[sel.selectedIndex];
    const isManual = opt && opt.dataset.manual === "1";
    if (isManual) {
      cfg.gain = manualGainFromShape(caps.manual_gain, cfg.gain);
    } else {
      // Carry the LNA state across the manual→AGC transition so
      // SDRplay backends that keep the LNA operator-controlled
      // (`keeps_lna_manual`) reuse the user's setpoint instead of
      // snapping back to the mid-band default. Pluto / DbContinuous
      // backends ignore the field anyway.
      const lna = readLnaStateFromGain(cfg.gain);
      cfg.gain = lna != null
        ? { kind: "agc_mode", id: sel.value, lna_state: lna }
        : { kind: "agc_mode", id: sel.value };
    }
    emit("settings:persist");
    refreshSdrPanels();   // re-render so the gain row's enable/disable matches.
  });
  label.appendChild(sel);
  row.appendChild(label);
  return row;
}

export function readLnaStateFromGain(gain) {
  if (!gain) return null;
  if (gain.kind === "manual" && gain.shape === "lna_plus_if" && Number.isFinite(gain.lna_state)) {
    return gain.lna_state;
  }
  if (gain.kind === "agc_mode" && Number.isFinite(gain.lna_state)) {
    return gain.lna_state;
  }
  return null;
}

export function agcModeKeepsLnaManual(caps, cfg) {
  if (!cfg.gain || cfg.gain.kind !== "agc_mode") return false;
  const mode = (caps.agc_modes || []).find(m => m.id === cfg.gain.id);
  return !!(mode && mode.keeps_lna_manual);
}

export function manualGainFromShape(shape, current) {
  if (current && current.kind === "manual") {
    if (shape && shape.DbContinuous && current.shape === "db") return current;
    if (shape && shape.LnaPlusIf && current.shape === "lna_plus_if") return current;
    if (shape && shape.DbDiscrete && current.shape === "discrete") return current;
  }
  if (shape && shape.DbContinuous) return { kind: "manual", shape: "db", db: 0 };
  if (shape && shape.LnaPlusIf) {
    // Carry the LNA state across the AGC→manual transition: SDRplay's
    // AGC mode keeps `lna_state` operator-controlled, so flipping back
    // to "Manuel (gain fixe)" should preserve it instead of snapping to
    // the mid-band default. IF gRdB starts back at 40 either way (the
    // daemon was managing it under AGC, we don't have a "last value").
    const lna = readLnaStateFromGain(current);
    return {
      kind: "manual",
      shape: "lna_plus_if",
      lna_state: lna != null ? lna : 4,
      if_grdb: 40,
    };
  }
  if (shape && shape.DbDiscrete) return { kind: "manual", shape: "discrete", step_idx: 0 };
  return { kind: "manual", shape: "db", db: 0 };
}

export function buildGainRow(direction, backendId, caps, cfg) {
  const row = makeRow();
  const shape = caps.manual_gain;
  const isAgc = !!(cfg.gain && cfg.gain.kind === "agc_mode");
  // For the LnaPlusIf shape (SDRplay), the LNA-state input stays
  // editable under AGC iff the active AGC mode advertises
  // `keeps_lna_manual`. The IF gRdB input is always daemon-managed
  // under AGC (only `disable` re-enables it via `manual: true`).
  const lnaStaysManual = isAgc && agcModeKeepsLnaManual(caps, cfg);
  if (shape && shape.DbContinuous) {
    const r = shape.DbContinuous;
    const label = document.createElement("label");
    label.className = "pluto-field";
    label.textContent = t(direction === "rx" ? "rx.dev_gain_rx" : "rx.dev_gain_tx");
    const input = document.createElement("input");
    input.type = "number";
    input.id = `sdr-${direction}-gain-db-${backendId}`;
    input.min = String(r.min_db); input.max = String(r.max_db); input.step = String(r.step_db);
    input.disabled = isAgc;
    if (cfg.gain && cfg.gain.kind === "manual" && cfg.gain.shape === "db") input.value = String(cfg.gain.db);
    input.dataset.sdrField = "gain.db";
    input.dataset.sdrTransform = "manual_db";
    input.dataset.backend = backendId;
    input.addEventListener("change", onSdrFieldChange);
    label.appendChild(input);
    row.appendChild(label);
  } else if (shape && shape.LnaPlusIf) {
    const r = shape.LnaPlusIf;
    const lnaLabel = document.createElement("label");
    lnaLabel.className = "pluto-field";
    lnaLabel.textContent = t("rx.dev_lna");
    const lnaInput = document.createElement("input");
    lnaInput.type = "number";
    lnaInput.id = `sdr-${direction}-gain-lna-${backendId}`;
    lnaInput.min = "0"; lnaInput.max = String(r.lna_states - 1); lnaInput.step = "1";
    lnaInput.disabled = isAgc && !lnaStaysManual;
    // Populate from manual.lna_state OR agc_mode.lna_state — both
    // shapes carry the operator's setpoint (cf. `readLnaStateFromGain`).
    const lnaCurrent = readLnaStateFromGain(cfg.gain);
    if (lnaCurrent != null) lnaInput.value = String(lnaCurrent);
    lnaInput.dataset.sdrField = "gain.lna_state";
    lnaInput.dataset.sdrTransform = "manual_lna";
    lnaInput.dataset.backend = backendId;
    lnaInput.addEventListener("change", onSdrFieldChange);
    lnaLabel.appendChild(lnaInput);
    row.appendChild(lnaLabel);

    const ifLabel = document.createElement("label");
    ifLabel.className = "pluto-field";
    ifLabel.textContent = t("rx.dev_if");
    const ifInput = document.createElement("input");
    ifInput.type = "number";
    ifInput.id = `sdr-${direction}-gain-if-${backendId}`;
    ifInput.min = String(r.if_grdb_range[0]); ifInput.max = String(r.if_grdb_range[1]); ifInput.step = String(r.if_grdb_step);
    ifInput.disabled = isAgc;
    if (cfg.gain && cfg.gain.kind === "manual" && cfg.gain.shape === "lna_plus_if") ifInput.value = String(cfg.gain.if_grdb);
    ifInput.dataset.sdrField = "gain.if_grdb";
    ifInput.dataset.sdrTransform = "manual_if";
    ifInput.dataset.backend = backendId;
    ifInput.addEventListener("change", onSdrFieldChange);
    ifLabel.appendChild(ifInput);
    row.appendChild(ifLabel);
  } else if (shape && shape.DbDiscrete) {
    // RTL-SDR-style ladder: one `<select>` of "<N> dB" options, indexed
    // by step_idx so the backend resolves the matching tenths-of-dB
    // gain value internally. Disabled when AGC is engaged (the tuner
    // drives the IF gain itself).
    const r = shape.DbDiscrete;
    const label = document.createElement("label");
    label.className = "pluto-field";
    label.textContent = t(direction === "rx" ? "rx.dev_gain_rx" : "rx.dev_gain_tx");
    const sel = document.createElement("select");
    sel.id = `sdr-${direction}-gain-step-${backendId}`;
    sel.disabled = isAgc;
    for (let i = 0; i < r.steps_db.length; i++) {
      const opt = document.createElement("option");
      opt.value = String(i);
      opt.textContent = `${r.steps_db[i]} dB`;
      sel.appendChild(opt);
    }
    let curIdx = 0;
    if (
      cfg.gain && cfg.gain.kind === "manual" && cfg.gain.shape === "discrete" &&
      Number.isFinite(cfg.gain.step_idx)
    ) {
      curIdx = Math.min(Math.max(0, cfg.gain.step_idx), r.steps_db.length - 1);
    }
    sel.value = String(curIdx);
    sel.dataset.sdrField = "gain.step_idx";
    sel.dataset.sdrTransform = "manual_step_idx";
    sel.dataset.backend = backendId;
    sel.addEventListener("change", onSdrFieldChange);
    label.appendChild(sel);
    row.appendChild(label);
  }
  return row;
}

export function buildAntennaRow(backendId, caps, cfg) {
  const row = makeRow();
  row.appendChild(makeFieldLabel("Port antenne :"));
  const sel = document.createElement("select");
  sel.id = `sdr-rx-antenna-${backendId}`;
  sel.dataset.sdrField = "antenna";
  sel.dataset.sdrTransform = "string";
  sel.dataset.backend = backendId;
  for (const a of caps.antennas) {
    const opt = document.createElement("option");
    opt.value = a.id;
    opt.textContent = a.label;
    sel.appendChild(opt);
  }
  if (cfg.antenna) sel.value = cfg.antenna;
  sel.addEventListener("change", onSdrFieldChange);
  row.appendChild(sel);
  return row;
}

export function buildFeatureRow(direction, backendId, caps, cfg) {
  const row = makeRow();
  if (caps.features.bias_t) row.appendChild(makeCheckbox(backendId, "bias_t", t("rx.dev_bias_t"), !!cfg.bias_t));
  if (caps.features.fm_notch) row.appendChild(makeCheckbox(backendId, "fm_notch", "Filtre rejet FM (88-108 MHz)", !!cfg.fm_notch));
  if (caps.features.dab_notch) row.appendChild(makeCheckbox(backendId, "dab_notch", "Filtre rejet DAB (174-240 MHz)", !!cfg.dab_notch));
  return row;
}

export function makeCheckbox(backendId, fieldName, labelText, checked) {
  const label = document.createElement("label");
  label.className = "pluto-field";
  const cb = document.createElement("input");
  cb.type = "checkbox"; cb.checked = checked;
  cb.dataset.sdrField = fieldName;
  cb.dataset.sdrTransform = "bool";
  cb.dataset.backend = backendId;
  cb.addEventListener("change", onSdrFieldChange);
  label.appendChild(cb);
  label.appendChild(document.createTextNode(" " + labelText));
  return label;
}

export function buildDeviationRow(direction, backendId, cfg) {
  const row = makeRow();
  row.appendChild(makeFieldLabel(t(direction === "rx" ? "rx.dev_dev_rx" : "rx.dev_dev_tx")));
  const fieldHz = direction === "rx" ? "max_deviation_hz" : "tx_deviation_hz";
  const cur = cfg[fieldHz] != null ? cfg[fieldHz] : 5000;
  for (const v of [5000, 2500]) {
    const label = document.createElement("label");
    label.className = "pluto-field";
    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = `sdr-${direction}-dev-${backendId}`;
    radio.value = String(v);
    radio.checked = (Math.round(cur) === v);
    radio.dataset.sdrField = fieldHz;
    radio.dataset.sdrTransform = "float";
    radio.dataset.backend = backendId;
    radio.addEventListener("change", onSdrFieldChange);
    label.appendChild(radio);
    label.appendChild(document.createTextNode(v === 5000 ? " 5 kHz" : " 2.5 kHz"));
    row.appendChild(label);
  }
  return row;
}

export function buildCtcssRow(backendId, cfg) {
  const row = makeRow();
  const cbLabel = document.createElement("label");
  cbLabel.className = "pluto-field";
  const cb = document.createElement("input");
  cb.type = "checkbox";
  cb.checked = (cfg.ctcss_freq_hz > 0);
  cb.dataset.backend = backendId;
  cbLabel.appendChild(cb);
  cbLabel.appendChild(document.createTextNode(" CTCSS (squelch relais)"));
  row.appendChild(cbLabel);
  const toneLabel = document.createElement("label");
  toneLabel.className = "pluto-field";
  toneLabel.textContent = t("rx.dev_tone");
  const sel = document.createElement("select");
  sel.id = `sdr-tx-ctcss-tone-${backendId}`;
  for (const f of EIA_CTCSS_TONES_HZ) {
    const opt = document.createElement("option");
    opt.value = String(f);
    opt.textContent = `${f.toFixed(1)} Hz`;
    sel.appendChild(opt);
  }
  const cur = cfg.ctcss_freq_hz > 0 ? cfg.ctcss_freq_hz : 88.5;
  const closest = EIA_CTCSS_TONES_HZ.reduce(
    (best, t) => (Math.abs(t - cur) < Math.abs(best - cur) ? t : best),
    EIA_CTCSS_TONES_HZ[0]);
  sel.value = String(closest);
  sel.addEventListener("change", () => {
    if (cb.checked) {
      cfg.ctcss_freq_hz = parseFloat(sel.value);
      emit("settings:persist");
    }
  });
  cb.addEventListener("change", () => {
    cfg.ctcss_freq_hz = cb.checked ? parseFloat(sel.value) : 0.0;
    emit("settings:persist");
  });
  toneLabel.appendChild(sel);
  row.appendChild(toneLabel);
  return row;
}

export function buildTxAttenuationRow(backendId, cfg) {
  const row = makeRow();
  const label = document.createElement("label");
  label.className = "pluto-field";
  label.textContent = t("rx.dev_atten_tx");
  const input = document.createElement("input");
  input.type = "number";
  input.id = `sdr-tx-att-${backendId}`;
  input.min = "0"; input.max = "89.75"; input.step = "0.25";
  const cur = (cfg.backend_extras && cfg.backend_extras.tx_attenuation_db != null)
    ? cfg.backend_extras.tx_attenuation_db : 30.0;
  input.value = String(cur);
  input.dataset.sdrField = "backend_extras.tx_attenuation_db";
  input.dataset.sdrTransform = "extras_float";
  input.dataset.backend = backendId;
  input.addEventListener("change", onSdrFieldChange);
  label.appendChild(input);
  row.appendChild(label);
  return row;
}

export function buildBackendExtrasRow(direction, backendId, caps, cfg) {
  // The tuner radio is now driven by the backend's caps —
  // multi-tuner devices (RSPduo) populate `tuner_options`;
  // single-tuner ones (RSP1x, Pluto) leave it empty so the row
  // disappears entirely. Other backend_extras keys
  // (decimation, tx_attenuation_db, …) stay managed under the hood
  // via `makeDefaultSdrConfig`.
  const row = makeRow();
  const tunerOptions = (caps && caps.tuner_options) || [];
  if (direction === "rx" && tunerOptions.length > 0) {
    row.appendChild(makeFieldLabel("Tuner :"));
    const persisted = cfg.backend_extras && cfg.backend_extras.tuner;
    // Prefer the persisted value when it matches one of the offered
    // IDs; otherwise default to the first option (the GUI used to
    // hardcode "B" for RSPduo — same end result via the order in
    // `BackendCapabilities::tuner_options`).
    const cur = tunerOptions.some(o => o.id === persisted)
      ? persisted
      : tunerOptions[0].id;
    for (const opt of tunerOptions) {
      const label = document.createElement("label");
      label.className = "pluto-field";
      const radio = document.createElement("input");
      radio.type = "radio";
      radio.name = `sdr-extras-tuner-${backendId}`;
      radio.value = opt.id;
      radio.checked = (cur === opt.id);
      radio.dataset.sdrField = "backend_extras.tuner";
      radio.dataset.sdrTransform = "extras_string";
      radio.dataset.backend = backendId;
      radio.addEventListener("change", onSdrFieldChange);
      label.appendChild(radio);
      label.appendChild(document.createTextNode(` ${opt.label}`));
      row.appendChild(label);
    }
  }
  return row;
}

export function onSdrFieldChange(evt) {
  const el = evt.currentTarget;
  const backendId = el.dataset.backend;
  if (!backendId) return;
  const cfg = ensureBackendConfig(backendId);
  applySdrFieldUpdate(cfg, el.dataset.sdrField, el.dataset.sdrTransform, el);
  emit("settings:persist");
}

export function applySdrFieldUpdate(cfg, field, transform, el) {
  switch (transform) {
    case "mhz_to_hz": {
      const m = parseFloat(el.value);
      if (Number.isFinite(m) && m > 0) cfg[field] = Math.round(m * 1e6);
      break;
    }
    case "string": cfg[field] = el.value; break;
    case "bool":   cfg[field] = !!el.checked; break;
    case "float": {
      const v = parseFloat(el.value);
      if (Number.isFinite(v)) cfg[field] = v;
      break;
    }
    case "manual_db": {
      const v = parseInt(el.value, 10);
      if (Number.isFinite(v)) cfg.gain = { kind: "manual", shape: "db", db: v };
      break;
    }
    case "manual_lna": {
      let v = parseInt(el.value, 10);
      if (!Number.isFinite(v)) break;
      // Clamp to the input's advertised range and write it back. The HTML
      // `max` attribute does NOT stop a typed value, and an out-of-range
      // LNA-state index crashes the SDRplay daemon (taking the SDR service
      // down), so we never let one through.
      const lnaLo = parseInt(el.min, 10);
      const lnaHi = parseInt(el.max, 10);
      if (Number.isFinite(lnaLo)) v = Math.max(lnaLo, v);
      if (Number.isFinite(lnaHi)) v = Math.min(lnaHi, v);
      el.value = String(v);
      // When AGC is engaged AND the mode keeps LNA operator-controlled
      // (SDRplay AGC), update the agc_mode variant's lna_state overlay
      // in place — don't switch back to a `manual` payload, that would
      // disengage the AGC. Otherwise the user is in pure manual mode
      // and we just patch the LnaPlusIf payload like before.
      if (cfg.gain && cfg.gain.kind === "agc_mode") {
        cfg.gain = { ...cfg.gain, lna_state: v };
      } else {
        const cur = (cfg.gain && cfg.gain.kind === "manual" && cfg.gain.shape === "lna_plus_if")
          ? cfg.gain : { kind: "manual", shape: "lna_plus_if", lna_state: 0, if_grdb: 40 };
        cfg.gain = { ...cur, lna_state: v };
      }
      break;
    }
    case "manual_if": {
      const v = parseInt(el.value, 10);
      if (Number.isFinite(v)) {
        const cur = (cfg.gain && cfg.gain.kind === "manual" && cfg.gain.shape === "lna_plus_if")
          ? cfg.gain : { kind: "manual", shape: "lna_plus_if", lna_state: 4, if_grdb: 40 };
        cfg.gain = { ...cur, if_grdb: v };
      }
      break;
    }
    case "manual_step_idx": {
      const v = parseInt(el.value, 10);
      if (Number.isFinite(v)) {
        cfg.gain = { kind: "manual", shape: "discrete", step_idx: v };
      }
      break;
    }
    case "extras_float": {
      const v = parseFloat(el.value);
      const key = field.split(".")[1];
      if (Number.isFinite(v)) {
        if (!cfg.backend_extras) cfg.backend_extras = {};
        cfg.backend_extras[key] = v;
      }
      break;
    }
    case "extras_string": {
      const key = field.split(".")[1];
      if (!cfg.backend_extras) cfg.backend_extras = {};
      cfg.backend_extras[key] = el.value;
      break;
    }
    default: break;
  }
}

export const FREQ_INPUT_ID_PREFIX = ["sdr-rx-freq-", "sdr-tx-freq-"];

export function isFreqInputId(id) {
  if (!id) return false;
  return FREQ_INPUT_ID_PREFIX.some(p => id.startsWith(p));
}

export function backendIdForFreqInput(targetEl) {
  if (!targetEl) return null;
  return targetEl.dataset && targetEl.dataset.backend
    ? targetEl.dataset.backend : null;
}

export function freqFavoritesArray(backendId) {
  if (!backendId) return [];
  ensureBackendEntry(backendId);
  const entry = currentSettings.sdr_settings.backends[backendId];
  if (!Array.isArray(entry.freq_favorites)) entry.freq_favorites = [];
  return entry.freq_favorites;
}

export async function pushFreqMru(mhz, targetEl) {
  if (!Number.isFinite(mhz) || mhz <= 0) return;
  const backendId = backendIdForFreqInput(targetEl);
  if (!backendId) return;
  const hz = Math.round(mhz * 1e6);
  const list = freqFavoritesArray(backendId);
  const idx = list.indexOf(hz);
  if (idx !== -1) list.splice(idx, 1);
  list.unshift(hz);
  while (list.length > 6) list.pop();
  if (window.__TAURI__ && window.__TAURI__.core) {
    try {
      await invoke("save_settings", { settings: currentSettings });
    } catch (err) {
      console.warn("save favorites:", err);
    }
  }
}
