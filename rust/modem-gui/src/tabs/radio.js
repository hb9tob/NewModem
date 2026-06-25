// Radio tab — SDR-only monitor: S-meter, RF/audio spectrum + waterfall, freq
// scale + click-to-tune (peak snap), gain/AGC controls and the SDR popover. Owns
// radioState; renders driven by the radio_spectrum/radio_audio/smeter/tune events
// (wired in main.js, which mutates radioState). Subscribes to sdr:refresh-* and
// capture:started on the bus so it never imports Settings/RX directly.
import { invoke } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "../lib/state.js";
import { getSelectedBackendId } from "../lib/dom.js";
import { escapeHtml } from "../lib/format.js";
import { on, emit } from "../lib/bus.js";
import { sdrBackends, getCapsForSelected, ensureBackendConfig, buildAgcRow, buildGainRow, buildAntennaRow, buildFeatureRow, buildBackendExtrasRow, hasFeatureToggles } from "../lib/sdr.js";
import { startCapture, stopCapture } from "../lib/capture.js";

export function ensureRadioSettings() {
  const r = currentSettings.radio || (currentSettings.radio = {});
  if (typeof r.squelch_enabled !== "boolean") r.squelch_enabled = false;
  if (!Number.isFinite(r.squelch_dbfs)) r.squelch_dbfs = -80;
  if (r.monitor_device === undefined) r.monitor_device = null;
  if (!Number.isFinite(r.monitor_volume)) r.monitor_volume = 0.80;
  if (!Number.isFinite(r.smeter_cal_trim_db)) r.smeter_cal_trim_db = 0;
  return r;
}

export const radioState = {
  rf: null, // latest RF SpectrumFrame {bins_db, center_hz, span_hz, seq}
  audio: null, // latest audio SpectrumFrame
  smeterDb: null, // EMA-smoothed channel power, dBFS
  tune: null, // latest TuneState
  rafId: null,
  waterfallInit: false,
};

export const RADIO_WF_PALETTE = (() => {
  const p = new Uint8ClampedArray(256 * 3);
  const stops = [
    [0, 0, 0, 0],
    [0.15, 0, 0, 80],
    [0.35, 0, 120, 180],
    [0.5, 0, 200, 160],
    [0.65, 120, 220, 40],
    [0.8, 240, 200, 0],
    [0.92, 240, 60, 0],
    [1.0, 255, 255, 255],
  ];
  for (let i = 0; i < 256; i++) {
    const f = i / 255;
    let a = stops[0];
    let b = stops[stops.length - 1];
    for (let s = 0; s < stops.length - 1; s++) {
      if (f >= stops[s][0] && f <= stops[s + 1][0]) {
        a = stops[s];
        b = stops[s + 1];
        break;
      }
    }
    const span = b[0] - a[0] || 1;
    const u = (f - a[0]) / span;
    p[i * 3] = a[1] + (b[1] - a[1]) * u;
    p[i * 3 + 1] = a[2] + (b[2] - a[2]) * u;
    p[i * 3 + 2] = a[3] + (b[3] - a[3]) * u;
  }
  return p;
})();

export function radioLevelRange() {
  const lo = parseFloat(document.getElementById("radio-level-min")?.value ?? "-120");
  const hi = parseFloat(document.getElementById("radio-level-max")?.value ?? "-20");
  return hi > lo ? [lo, hi] : [lo, lo + 1];
}

export const PLUTO_FS_DBM = 0; // input power for 0 dBFS at 0 dB gain (approx.)

export const SMETER_S9_DBM = -93;

export const SMETER_DB_PER_UNIT = 6;

export function radioGainDb() {
  const backendId = getSelectedBackendId("rx-device-select");
  const cfg = backendId ? ensureBackendConfig(backendId) : null;
  const g = cfg && cfg.gain;
  if (g && g.kind === "manual") {
    if (g.shape === "db" && Number.isFinite(g.db)) return g.db;
    if (g.shape === "lna_plus_if" && Number.isFinite(g.if_grdb)) {
      // gRdB is a gain *reduction* (20..59): higher = less gain. LNA state
      // ignored (no gain table here) — the trim covers the residual.
      return 59 - g.if_grdb;
    }
    if (g.shape === "discrete" && Number.isFinite(g.step_idx)) {
      const ladder = getCapsForSelected("rx-device-select")?.manual_gain?.DbDiscrete?.steps_db;
      if (ladder && ladder.length) {
        return ladder[Math.min(Math.max(0, g.step_idx), ladder.length - 1)];
      }
    }
  }
  // AGC (or unknown shape): hardware gain isn't reported back — mid estimate.
  return 30;
}

export function radioCalTrimDb() {
  return parseFloat(document.getElementById("radio-cal-trim")?.value ?? "0");
}

export function radioDbfsToDbm(dbfs) {
  return dbfs - radioGainDb() + PLUTO_FS_DBM + radioCalTrimDb();
}

export function radioDbmToSUnits(dbm) {
  return 9 + (dbm - SMETER_S9_DBM) / SMETER_DB_PER_UNIT;
}

export function sizeRadioCanvas(canvas) {
  if (!canvas) return null;
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.floor(canvas.clientWidth * dpr));
  const h = Math.max(1, Math.floor(canvas.clientHeight * dpr));
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
    radioState.waterfallInit = false; // RF/waterfall geometry changed
  }
  return canvas.getContext("2d");
}

export async function invokeRadio(cmd, args) {
  try {
    await invoke(cmd, args || {});
  } catch (err) {
    // No active SDR session yet (capture not started) → soft-ignore.
    if (!String(err).includes("Radio indisponible")) {
      console.warn(`[radio] ${cmd}`, err);
    }
  }
}

export function refreshRadioTabVisibility() {
  const btn = document.getElementById("tab-btn-radio");
  if (!btn) return;
  const isSdr = getSelectedBackendId("rx-device-select") !== null;
  btn.hidden = !isSdr;
  if (!isSdr) {
    // If we were on the Radio tab and the source changed to non-SDR,
    // fall back to RX.
    if (btn.classList.contains("active")) {
      document.querySelector('.tab-bar .tab[data-tab="rx"]')?.click();
    }
  }
}

export function updateRadioTuneDisplay() {
  const t = radioState.tune;
  const disp = document.getElementById("radio-freq-display");
  const info = document.getElementById("radio-tune-info");
  if (t && disp) {
    disp.textContent = `${(t.displayed_rf_hz / 1e6).toFixed(6)} MHz`;
  }
  if (t && info) {
    const off = t.digital_offset_hz;
    const offStr = `${off >= 0 ? "+" : ""}${(off / 1000).toFixed(2)} kHz`;
    info.textContent = `OL ${(t.lo_hz / 1e6).toFixed(4)} MHz · décalage num. ${offStr} · span ${(t.input_rate_hz / 1000).toFixed(0)} kHz`;
  }
}

export function currentRadioHz() {
  if (radioState.tune) return radioState.tune.displayed_rf_hz;
  const v = parseFloat(document.getElementById("radio-freq-input")?.value);
  return isFinite(v) ? Math.round(v * 1e6) : 145_500_000;
}

export function tuneRadioTo(hz) {
  const clamped = Math.max(0, Math.round(hz));
  const input = document.getElementById("radio-freq-input");
  if (input) input.value = (clamped / 1e6).toFixed(6);
  invokeRadio("set_radio_freq", { hz: clamped });
  // Persist the dialed RF into the active RX backend's config so a later
  // plain "Start RX" (which reads the saved SDR config) reuses the last
  // frequency tuned here — the Settings RX panel no longer carries this
  // field now that all SDR RX controls live on the Radio tab.
  const backendId = getSelectedBackendId("rx-device-select");
  if (backendId) {
    const cfg = ensureBackendConfig(backendId);
    cfg.rx_freq_hz = clamped;
    emit("sdr:persist-settings");
  }
}

export async function setupRadioTab() {
  // Own the Radio reactions to shared-bus events (SDR gain/params changes from
  // Settings, and an SDR capture coming up) so main.js never references the
  // Radio renderers. Mirrors the calls lib/sdr.js + lib/capture.js emit.
  on("sdr:refresh-radio-gain", () => renderRadioGain());
  on("sdr:refresh-radio-sdr-params", () => renderRadioSdrParams());
  on("capture:started", () => pushRadioControlsLive());
  // Settings emits this when the RX device changes (Radio tab is SDR-only).
  on("rx-device:changed", () => refreshRadioTabVisibility());
  // Frequency entry + tune button.
  document.getElementById("radio-freq-set")?.addEventListener("click", () => {
    const v = parseFloat(document.getElementById("radio-freq-input").value);
    if (isFinite(v)) tuneRadioTo(v * 1e6);
  });
  document.getElementById("radio-freq-input")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      const v = parseFloat(e.target.value);
      if (isFinite(v)) tuneRadioTo(v * 1e6);
    }
  });
  // Step buttons (digital fine-tune in the common case).
  for (const b of document.querySelectorAll(".radio-step")) {
    b.addEventListener("click", () => {
      tuneRadioTo(currentRadioHz() + Number(b.dataset.step));
    });
  }
  // Amateur-band presets.
  for (const b of document.querySelectorAll(".radio-band")) {
    b.addEventListener("click", () => tuneRadioTo(Number(b.dataset.freq)));
  }
  document.getElementById("radio-recenter")?.addEventListener("click", () => {
    invokeRadio("recenter_lo");
  });

  // RX gain / AGC — backend-aware controls built by renderRadioGain() from
  // the active SDR's capabilities. A single delegated `change` listener on
  // the host sends the new gain live (set_radio_gain): the builders mutate
  // cfg.gain and bubble their change event up to us, so cfg is current here.
  const gainHost = document.getElementById("radio-gain-host");
  if (gainHost) gainHost.addEventListener("change", sendRadioGainLive);
  renderRadioGain();

  // Deviation / channel width. Apply live and persist into the active RX
  // backend's config (max_deviation_hz) — same rationale as tuneRadioTo:
  // the Settings RX panel no longer carries this field.
  document.getElementById("radio-deviation")?.addEventListener("change", (e) => {
    const hz = Number(e.target.value);
    invokeRadio("set_deviation", { hz });
    const backendId = getSelectedBackendId("rx-device-select");
    if (backendId) {
      const cfg = ensureBackendConfig(backendId);
      cfg.max_deviation_hz = hz;
      emit("sdr:persist-settings");
    }
  });

  // Squelch.
  const sqOn = document.getElementById("radio-squelch-on");
  const sqDb = document.getElementById("radio-squelch-db");
  const sqLabel = document.getElementById("radio-squelch-label");
  const sendSquelch = () => {
    if (sqLabel) sqLabel.textContent = `${sqDb.value} dB`;
    invokeRadio("set_squelch", {
      dbfs: Number(sqDb.value),
      enabled: !!sqOn.checked,
    });
    const r = ensureRadioSettings();
    r.squelch_enabled = !!sqOn.checked;
    r.squelch_dbfs = Number(sqDb.value);
    emit("sdr:persist-settings");
  };
  sqOn?.addEventListener("change", sendSquelch);
  sqDb?.addEventListener("input", () => {
    if (sqLabel) sqLabel.textContent = `${sqDb.value} dB`;
  });
  sqDb?.addEventListener("change", sendSquelch);

  // S-meter calibration trim. Read live by drawRadioSmeter — purely a
  // display offset, no backend round-trip.
  const calTrim = document.getElementById("radio-cal-trim");
  const calLabel = document.getElementById("radio-cal-trim-label");
  const fmtTrim = () => {
    if (calLabel) {
      const v = Number(calTrim.value);
      calLabel.textContent = `${v > 0 ? "+" : ""}${v} dB`;
    }
  };
  calTrim?.addEventListener("input", fmtTrim);
  calTrim?.addEventListener("change", () => {
    ensureRadioSettings().smeter_cal_trim_db = Number(calTrim.value);
    emit("sdr:persist-settings");
  });
  fmtTrim();

  // Click-to-tune on the RF spectrum and the waterfall: snap to the actual
  // signal. The clicked x maps to a frequency, then we pull onto the
  // strongest RF bin within a small window around it (so a slightly-off
  // click still lands on the carrier), and round to the nearest kHz for a
  // clean readout. No channel raster — stays correct for the future SSB /
  // QO-100 linear-transponder mode. Fine-trim with the ±1k buttons.
  for (const id of ["radio-rf-fft", "radio-waterfall"]) {
    const cv = document.getElementById(id);
    cv?.addEventListener("click", (e) => {
      const hz = radioSnapHz(cv, e.clientX);
      if (hz !== null) tuneRadioTo(hz);
    });
  }

  // Monitor output device list + selection.
  const monSel = document.getElementById("radio-monitor-out");
  if (monSel) {
    try {
      const outs = await invoke("list_output_audio_devices");
      for (const d of outs || []) {
        const o = document.createElement("option");
        o.value = d.name;
        o.textContent = d.name;
        monSel.appendChild(o);
      }
    } catch (err) {
      console.warn("[radio] list_output_audio_devices", err);
    }
    monSel.addEventListener("change", () => {
      invokeRadio("set_monitor_output", { device: monSel.value || null });
      ensureRadioSettings().monitor_device = monSel.value || null;
      // Push the current volume right after enabling a device.
      if (monSel.value) {
        const vol = Number(document.getElementById("radio-volume")?.value ?? 80) / 100;
        invokeRadio("set_monitor_volume", { gain: vol });
      }
      emit("sdr:persist-settings");
    });
  }
  const vol = document.getElementById("radio-volume");
  const volLabel = document.getElementById("radio-volume-label");
  vol?.addEventListener("input", () => {
    if (volLabel) volLabel.textContent = `${vol.value} %`;
    invokeRadio("set_monitor_volume", { gain: Number(vol.value) / 100 });
  });
  // Persist on commit only (not every input tick → no disk-write storm).
  vol?.addEventListener("change", () => {
    ensureRadioSettings().monitor_volume = Number(vol.value) / 100;
    emit("sdr:persist-settings");
  });

  // Restore the persisted control values now that the monitor list and
  // every listener are in place.
  restoreRadioControls();
}

export function restoreRadioControls() {
  const r = ensureRadioSettings();
  const sqOn = document.getElementById("radio-squelch-on");
  const sqDb = document.getElementById("radio-squelch-db");
  const sqLabel = document.getElementById("radio-squelch-label");
  if (sqOn) sqOn.checked = !!r.squelch_enabled;
  if (sqDb) sqDb.value = String(r.squelch_dbfs);
  if (sqLabel) sqLabel.textContent = `${r.squelch_dbfs} dB`;

  const calTrim = document.getElementById("radio-cal-trim");
  const calLabel = document.getElementById("radio-cal-trim-label");
  if (calTrim) calTrim.value = String(r.smeter_cal_trim_db);
  if (calLabel) {
    const v = Number(r.smeter_cal_trim_db);
    calLabel.textContent = `${v > 0 ? "+" : ""}${v} dB`;
  }

  const vol = document.getElementById("radio-volume");
  const volLabel = document.getElementById("radio-volume-label");
  const pct = Math.round(r.monitor_volume * 100);
  if (vol) vol.value = String(pct);
  if (volLabel) volLabel.textContent = `${pct} %`;

  // The monitor device option only exists once list_output_audio_devices
  // has populated the select; setting an absent value is a no-op (stays
  // "off"), which is the safe fallback when the device is unplugged.
  const monSel = document.getElementById("radio-monitor-out");
  if (monSel && r.monitor_device) monSel.value = r.monitor_device;
}

export function pushRadioControlsLive() {
  const r = ensureRadioSettings();
  invokeRadio("set_squelch", { dbfs: r.squelch_dbfs, enabled: !!r.squelch_enabled });
  if (r.monitor_device) {
    invokeRadio("set_monitor_output", { device: r.monitor_device });
    invokeRadio("set_monitor_volume", { gain: r.monitor_volume });
  }
}

export function renderRadioGain() {
  const host = document.getElementById("radio-gain-host");
  if (!host) return;
  // Drop previously-built rows, keep the leading "Gain RX" label span.
  host.querySelectorAll(".pluto-row").forEach((n) => n.remove());
  const backendId = getSelectedBackendId("rx-device-select");
  const info = backendId ? sdrBackends.get(backendId) : null;
  if (!info) return;
  const caps = getCapsForSelected("rx-device-select") || info.capabilities;
  const cfg = ensureBackendConfig(backendId);
  if (caps.agc_modes && caps.agc_modes.length > 0) {
    host.appendChild(buildAgcRow("rx", backendId, caps, cfg));
  }
  host.appendChild(buildGainRow("rx", backendId, caps, cfg));
}

export function sendRadioGainLive() {
  const backendId = getSelectedBackendId("rx-device-select");
  if (!backendId) return;
  const cfg = ensureBackendConfig(backendId);
  if (cfg && cfg.gain) invokeRadio("set_radio_gain", { gain: cfg.gain });
}

export function renderRadioSdrParams() {
  const rows = document.getElementById("radio-sdr-rows");
  if (!rows) return;
  const backendId = getSelectedBackendId("rx-device-select");
  const info = backendId ? sdrBackends.get(backendId) : null;
  if (!info) {
    rows.innerHTML = `<span class="tx-hint">${escapeHtml(t("radio.sdr_params_unavailable"))}</span>`;
    return;
  }
  const caps = getCapsForSelected("rx-device-select") || info.capabilities;
  const cfg = ensureBackendConfig(backendId);
  rows.innerHTML = "";
  // Gain / AGC live on the Radio tab's control bar (renderRadioGain), not
  // here — keeping them in both places caused conflicting values.
  if (caps.antennas && caps.antennas.length > 0) {
    rows.appendChild(buildAntennaRow(backendId, caps, cfg));
  }
  if (hasFeatureToggles(caps)) {
    rows.appendChild(buildFeatureRow("rx", backendId, caps, cfg));
  }
  rows.appendChild(buildBackendExtrasRow("rx", backendId, caps, cfg));
}

export function openRadioSdrModal() {
  const modal = document.getElementById("radio-sdr-modal");
  if (!modal) return;
  renderRadioSdrParams();
  modal.hidden = false;
}

export function closeRadioSdrModal() {
  const modal = document.getElementById("radio-sdr-modal");
  if (modal) modal.hidden = true;
}

export async function restartRadioCapture() {
  closeRadioSdrModal();
  await stopCapture();
  await startCapture();
}

export function setupRadioSdrModal() {
  const btn = document.getElementById("radio-sdr-params-btn");
  const modal = document.getElementById("radio-sdr-modal");
  if (!btn || !modal) return;
  btn.addEventListener("click", openRadioSdrModal);
  document.getElementById("radio-sdr-close")?.addEventListener("click", closeRadioSdrModal);
  modal.addEventListener("click", (e) => {
    if (e.target === modal) closeRadioSdrModal();
  });
  document.addEventListener("keydown", (e) => {
    if (!modal.hidden && e.key === "Escape") {
      e.preventDefault();
      closeRadioSdrModal();
    }
  });
  document
    .getElementById("radio-sdr-restart")
    ?.addEventListener("click", () => {
      restartRadioCapture().catch((err) => console.error("[radio] restart", err));
    });
}

export function startRadioRender() {
  refreshRadioTabVisibility();
  renderRadioGain();
  seedRadioFreqInput();
  pushRadioControlsLive();
  if (radioState.rafId !== null) return;
  radioState.waterfallInit = false;
  const loop = () => {
    renderRadio();
    radioState.rafId = requestAnimationFrame(loop);
  };
  radioState.rafId = requestAnimationFrame(loop);
}

export function seedRadioFreqInput() {
  const backendId = getSelectedBackendId("rx-device-select");
  if (!backendId) return;
  const cfg = ensureBackendConfig(backendId);
  // Frequency: only when no live session is driving the display and the
  // operator isn't mid-edit (blank input) — never clobber either.
  const input = document.getElementById("radio-freq-input");
  if (input && !input.value && !radioState.tune &&
      Number.isFinite(cfg.rx_freq_hz) && cfg.rx_freq_hz > 0) {
    input.value = (cfg.rx_freq_hz / 1e6).toFixed(6);
  }
  // Channel width: reflect the persisted deviation (not session-driven).
  const dev = document.getElementById("radio-deviation");
  if (dev && Number.isFinite(cfg.max_deviation_hz)) {
    dev.value = String(Math.round(cfg.max_deviation_hz));
  }
}

export function stopRadioRender() {
  if (radioState.rafId !== null) {
    cancelAnimationFrame(radioState.rafId);
    radioState.rafId = null;
  }
}

export function renderRadio() {
  drawRadioSmeter();
  drawRadioSpectrum("radio-audio-fft", radioState.audio, "#29B6F6");
  drawRadioRf();
}

export const SMETER_DIAL_MIN_U = radioDbmToSUnits(SMETER_S9_DBM - 8 * SMETER_DB_PER_UNIT); // S1

export const SMETER_DIAL_MAX_U = radioDbmToSUnits(SMETER_S9_DBM + 60); // S9+60 dB

export const SMETER_ARC_START = Math.PI * 1.15;

export const SMETER_ARC_SWEEP = Math.PI * 0.7;

export function smeterFrac(sUnits) {
  return Math.max(
    0,
    Math.min(1, (sUnits - SMETER_DIAL_MIN_U) / (SMETER_DIAL_MAX_U - SMETER_DIAL_MIN_U)),
  );
}

export function drawRadioSmeter() {
  const canvas = document.getElementById("radio-smeter");
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  const cx = w / 2;
  const cy = h * 0.92;
  const r = Math.min(w * 0.42, h * 0.78);
  // Dial arc.
  ctx.strokeStyle = "#888";
  ctx.lineWidth = Math.max(1, w * 0.004);
  ctx.beginPath();
  ctx.arc(cx, cy, r, SMETER_ARC_START, SMETER_ARC_START + SMETER_ARC_SWEEP);
  ctx.stroke();
  // Calibrated ticks: S1,3,5,7,9 then +20/+40/+60 dB over S9, each at its
  // true dB position on the arc.
  ctx.fillStyle = "#aaa";
  // Cap the tick-label size: on the Pi 7" panel the card stretches the
  // canvas tall, and an uncapped h*0.1 made the S-unit digits oversized.
  ctx.font = `${Math.max(7, Math.min(11, Math.round(h * 0.1)))}px sans-serif`;
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  const ticks = [
    { label: "1", dbm: SMETER_S9_DBM - 8 * SMETER_DB_PER_UNIT, red: false },
    { label: "3", dbm: SMETER_S9_DBM - 6 * SMETER_DB_PER_UNIT, red: false },
    { label: "5", dbm: SMETER_S9_DBM - 4 * SMETER_DB_PER_UNIT, red: false },
    { label: "7", dbm: SMETER_S9_DBM - 2 * SMETER_DB_PER_UNIT, red: false },
    { label: "9", dbm: SMETER_S9_DBM, red: false },
    { label: "+20", dbm: SMETER_S9_DBM + 20, red: true },
    { label: "+40", dbm: SMETER_S9_DBM + 40, red: true },
    { label: "+60", dbm: SMETER_S9_DBM + 60, red: true },
  ];
  for (const tk of ticks) {
    const frac = smeterFrac(radioDbmToSUnits(tk.dbm));
    const ang = SMETER_ARC_START + frac * SMETER_ARC_SWEEP;
    const x1 = cx + Math.cos(ang) * r;
    const y1 = cy + Math.sin(ang) * r;
    const x2 = cx + Math.cos(ang) * (r * 0.9);
    const y2 = cy + Math.sin(ang) * (r * 0.9);
    ctx.strokeStyle = tk.red ? "#e53935" : "#888";
    ctx.beginPath();
    ctx.moveTo(x1, y1);
    ctx.lineTo(x2, y2);
    ctx.stroke();
    const lx = cx + Math.cos(ang) * (r * 0.76);
    const ly = cy + Math.sin(ang) * (r * 0.76);
    ctx.fillStyle = tk.red ? "#ef9a9a" : "#aaa";
    ctx.fillText(tk.label, lx, ly);
  }
  // Needle position from the calibrated S-unit value.
  const db = radioState.smeterDb;
  const dbm = db === null ? null : radioDbfsToDbm(db);
  const sUnits = dbm === null ? null : radioDbmToSUnits(dbm);
  const frac = sUnits === null ? 0 : smeterFrac(sUnits);
  const ang = SMETER_ARC_START + frac * SMETER_ARC_SWEEP;
  ctx.strokeStyle = "#e0e0e0";
  ctx.lineWidth = Math.max(1.5, w * 0.008);
  ctx.beginPath();
  ctx.moveTo(cx, cy);
  ctx.lineTo(cx + Math.cos(ang) * r * 0.95, cy + Math.sin(ang) * r * 0.95);
  ctx.stroke();
  ctx.textBaseline = "alphabetic";
  const label = document.getElementById("radio-smeter-label");
  if (label) {
    if (db === null) {
      label.textContent = "—";
    } else {
      label.textContent = `${smeterReport(sUnits)} · ${dbm.toFixed(0)} dBm`;
    }
  }
}

export function smeterReport(sUnits) {
  if (sUnits >= 9) {
    const over = Math.round((sUnits - 9) * SMETER_DB_PER_UNIT);
    return over <= 0 ? "S9" : `S9+${over} dB`;
  }
  return `S${Math.max(0, Math.round(sUnits))}`;
}

export function drawRadioSpectrum(canvasId, frame, color) {
  const canvas = document.getElementById(canvasId);
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = "#0a0a0a";
  ctx.fillRect(0, 0, w, h);
  if (!frame || !frame.bins_db || !frame.bins_db.length) return;
  const bins = frame.bins_db;
  const n = bins.length;
  const [lo, hi] = radioLevelRange();
  const span = hi - lo;
  ctx.strokeStyle = color;
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let x = 0; x < w; x++) {
    const bi = Math.min(n - 1, Math.floor((x / w) * n));
    const v = Math.max(0, Math.min(1, (bins[bi] - lo) / span));
    const y = h - v * h;
    if (x === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

export function niceTickStep(span, targetTicks) {
  const raw = span / Math.max(1, targetTicks);
  const mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const norm = raw / mag;
  let step;
  if (norm < 1.5) step = 1;
  else if (norm < 3) step = 2;
  else if (norm < 7) step = 5;
  else step = 10;
  return step * mag;
}

export function radioRfTicks(frame) {
  const lo = frame.center_hz - frame.span_hz / 2;
  const hi = frame.center_hz + frame.span_hz / 2;
  const step = niceTickStep(frame.span_hz, 6);
  const ticks = [];
  for (let f = Math.ceil(lo / step) * step; f <= hi + 1; f += step) ticks.push(f);
  return { lo, hi, ticks };
}

export function drawRadioFreqScale() {
  const canvas = document.getElementById("radio-freq-scale");
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = "#101216";
  ctx.fillRect(0, 0, w, h);
  const frame = radioState.rf;
  if (!frame || !frame.span_hz) return;
  const { lo, hi, ticks } = radioRfTicks(frame);
  const span = hi - lo || 1;
  // Fixed ~10 px CSS font, decoupled from the canvas buffer height so the
  // labels stay small even if the strip is ever laid out taller than its
  // 16 px CSS cap. `h` is device px (CSS px × dpr).
  const dpr = window.devicePixelRatio || 1;
  const fontPx = Math.round(10 * dpr);
  ctx.font = `${fontPx}px sans-serif`;
  ctx.textBaseline = "middle";
  const midY = h / 2;
  const tickY = Math.min(h, 4 * dpr);
  for (let i = 0; i < ticks.length; i++) {
    const f = ticks[i];
    const x = ((f - lo) / span) * w;
    const isCenter = Math.abs(f - frame.center_hz) < span * 0.01;
    ctx.strokeStyle = isCenter ? "#7fd1ff" : "#3a3d44";
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, tickY);
    ctx.stroke();
    ctx.fillStyle = isCenter ? "#7fd1ff" : "#9aa0aa";
    ctx.textAlign = x < w * 0.06 ? "left" : x > w * 0.94 ? "right" : "center";
    // "MHz" unit only on the last tick to keep the ruler uncluttered.
    const lbl =
      i === ticks.length - 1 ? `${(f / 1e6).toFixed(3)} MHz` : (f / 1e6).toFixed(3);
    ctx.fillText(lbl, x, midY + 1.5 * dpr);
  }
}

export function radioMarkerFrac() {
  const frame = radioState.rf;
  const t = radioState.tune;
  if (!frame || !frame.span_hz) return 0.5;
  const refHz = t ? t.displayed_rf_hz : frame.center_hz;
  return Math.max(0, Math.min(1, 0.5 + (refHz - frame.center_hz) / frame.span_hz));
}

export function radioFreqAtX(canvas, clientX) {
  const frame = radioState.rf;
  if (!canvas || !frame || !frame.span_hz) return null;
  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0) return null;
  const frac = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
  return frame.center_hz + (frac - 0.5) * frame.span_hz;
}

export const SNAP_WINDOW_HZ = 8_000; // search half-window around the click

export const SNAP_MIN_DB = 6; // peak must beat the local mean by this to snap

export function radioSnapHz(canvas, clientX) {
  const clickHz = radioFreqAtX(canvas, clientX);
  if (clickHz === null) return null;
  const frame = radioState.rf;
  if (frame && frame.bins_db && frame.bins_db.length) {
    const bins = frame.bins_db;
    const n = bins.length;
    const hzPerBin = frame.span_hz / n;
    const loEdge = frame.center_hz - frame.span_hz / 2;
    const clamp = (b) => Math.max(0, Math.min(n - 1, b));
    const clickBin = clamp(Math.round((clickHz - loEdge) / hzPerBin));
    const half = Math.max(1, Math.round(SNAP_WINDOW_HZ / hzPerBin));
    const a = clamp(clickBin - half);
    const b1 = clamp(clickBin + half);
    let bestBin = clickBin;
    let bestVal = -Infinity;
    let sum = 0;
    let cnt = 0;
    for (let b = a; b <= b1; b++) {
      const v = bins[b];
      sum += v;
      cnt++;
      if (v > bestVal) {
        bestVal = v;
        bestBin = b;
      }
    }
    const mean = cnt ? sum / cnt : -Infinity;
    // Snap to the peak only when one genuinely stands out.
    if (bestVal - mean >= SNAP_MIN_DB) {
      // Parabolic (quadratic) interpolation around the peak for sub-bin
      // accuracy: δ = ½·(yₗ−yᵣ)/(yₗ−2y꜀+yᵣ), valid for a concave top.
      let refined = bestBin;
      if (bestBin > 0 && bestBin < n - 1) {
        const yl = bins[bestBin - 1];
        const yc = bins[bestBin];
        const yr = bins[bestBin + 1];
        const denom = yl - 2 * yc + yr;
        if (denom < 0) {
          const delta = (0.5 * (yl - yr)) / denom;
          if (Math.abs(delta) <= 1) refined = bestBin + delta;
        }
      }
      const hz = loEdge + refined * hzPerBin;
      return Math.round(hz / 100) * 100;
    }
  }
  // Fallback: tune exactly where the user clicked, rounded to the kHz.
  return Math.round(clickHz / 1000) * 1000;
}

export function drawRadioRf() {
  // Top: line spectrum (with frequency gridlines). Middle: shared Hz
  // ruler. Bottom: scrolling waterfall — all three share the same
  // horizontal Hz mapping from the latest RF frame.
  drawRadioSpectrum("radio-rf-fft", radioState.rf, "#9CCC65");
  drawRfGridlines("radio-rf-fft", radioState.rf);
  drawTunedMarker("radio-rf-fft");
  drawRadioFreqScale();
  const canvas = document.getElementById("radio-waterfall");
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  if (!radioState.waterfallInit) {
    ctx.fillStyle = "#000";
    ctx.fillRect(0, 0, w, h);
    radioState.waterfallInit = true;
    radioState.lastWfSeq = -1;
  }
  const frame = radioState.rf;
  if (!frame || !frame.bins_db || !frame.bins_db.length) return;
  if (frame.seq === radioState.lastWfSeq) return; // no new frame
  radioState.lastWfSeq = frame.seq;
  // Scroll everything up by one pixel, then draw the newest row at the
  // bottom (scroll-blit + single new row — never a full re-render).
  ctx.drawImage(canvas, 0, 0, w, h, 0, -1, w, h);
  const bins = frame.bins_db;
  const n = bins.length;
  const [lo, hi] = radioLevelRange();
  const span = hi - lo;
  // Tuned-frequency column: tracks displayed_rf within the LO-centred band.
  const centerX = Math.round((w - 1) * radioMarkerFrac());
  const row = ctx.createImageData(w, 1);
  for (let x = 0; x < w; x++) {
    const bi = Math.min(n - 1, Math.floor((x / w) * n));
    const v = Math.max(0, Math.min(1, (bins[bi] - lo) / span));
    const idx = Math.min(255, Math.max(0, Math.round(v * 255)));
    if (x === centerX) {
      // Tint the centre column so a continuous tuned-frequency marker
      // scrolls down with the waterfall.
      row.data[x * 4] = 127;
      row.data[x * 4 + 1] = 209;
      row.data[x * 4 + 2] = 255;
    } else {
      row.data[x * 4] = RADIO_WF_PALETTE[idx * 3];
      row.data[x * 4 + 1] = RADIO_WF_PALETTE[idx * 3 + 1];
      row.data[x * 4 + 2] = RADIO_WF_PALETTE[idx * 3 + 2];
    }
    row.data[x * 4 + 3] = 255;
  }
  ctx.putImageData(row, 0, h - 1);
}
