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
  if (!Number.isFinite(r.fft_smooth_pct)) r.fft_smooth_pct = 50;
  if (!Number.isFinite(r.hang_ms)) r.hang_ms = 225;
  if (!Number.isFinite(r.decay_db_s)) r.decay_db_s = 55;
  if (!Number.isFinite(r.level_min_dbfs)) r.level_min_dbfs = -120;
  if (!Number.isFinite(r.level_max_dbfs)) r.level_max_dbfs = -20;
  return r;
}

export const radioState = {
  rf: null, // latest RF SpectrumFrame {bins_db, center_hz, span_hz, seq}
  audio: null, // latest audio SpectrumFrame
  smeterDb: null, // EMA-smoothed channel power, dBFS
  tune: null, // latest TuneState
  excursion: null, // latest FM-excursion frame {peak_hz, rms_hz, max_dev_hz, seq}
  excHist: [], // rolling history of {peak, rms} (Hz) for the scrolling graph
  excLastSeq: -1, // last ingested excursion seq (dedup vs the 60 Hz RAF)
  demodMode: "nbfm", // "nbfm" | "ssb_usb" — selects the meter drawn
  audioLevel: null, // latest SSB level frame {peak, rms, seq} (linear, 1.0 = full scale)
  levelHist: [], // rolling history of {peak, rms} (linear) for the SSB level graph
  levelLastSeq: -1, // last ingested SSB-level seq
  rafId: null,
  bandPlanQo100: false, // QO-100 NB transponder band-plan overlay active
  rfSmoothed: null, // EMA-averaged copy of the latest RF frame (display only)
  rfSmoothSeq: -1, // last RF seq folded into rfSmoothed
  rfHeld: null, // peak hang/decay envelope of the RF line spectrum (display only)
  rfDisplayLastMs: 0, // performance.now() of the last hang/decay tick
  tuning: false, // CW tune carrier active (TX)
  tuneTimer: null, // client-side 30 s auto-stop timer id
  waterfallInit: false,
  wfW: 0, // waterfall canvas dims last init'd at — re-init only when these change
  wfH: 0,
};

// QO-100 (Es'hail-2) narrowband transponder reference frequencies (sky/downlink),
// the standard universal-LNB local oscillator, and the constant uplink shift.
// Beacons are the firm anchors (AMSAT-DL band plan); TX = sky_RX + shift.
export const QO100_DOWNLINK_CENTER_HZ = 10_489_750_000; // middle BPSK beacon = passband centre
export const QO100_BEACON_LOWER_HZ = 10_489_500_000; // lower CW beacon (lower edge)
export const QO100_BEACON_UPPER_HZ = 10_490_000_000; // upper CW beacon (upper edge)
export const QO100_LNB_LO_HZ = 9_750_000_000; // standard universal LNB local oscillator
export const QO100_SHIFT_HZ = -8_089_500_000; // uplink = downlink − 8089.5 MHz
export const QO100_RF_BANDWIDTH_HZ = 540_000; // analog RF filter to pass the full ~500 kHz transponder

/// The active RX backend's persisted config, or null when no SDR is selected.
export function activeRadioCfg() {
  const backendId = getSelectedBackendId("rx-device-select");
  return backendId ? ensureBackendConfig(backendId) : null;
}

/// True while a live RX capture is running (the Stop button is enabled).
export function isCapturing() {
  const b = document.getElementById("btn-stop");
  return !!b && !b.disabled;
}

/// LNB display offset in Hz: when the LNB is enabled, the operator dials the real
/// sky frequency while the SDR tunes IF = sky − lnb_lo. Returns 0 when disabled
/// or unset (→ behaviour identical to plain terrestrial tuning).
export function skyOffsetHz(cfg) {
  const ex = cfg && cfg.backend_extras;
  if (!ex || !ex.lnb_enabled) return 0;
  const lo = Number(ex.lnb_lo_hz);
  return Number.isFinite(lo) ? lo : 0;
}

export function currentSkyOffsetHz() {
  return skyOffsetHz(activeRadioCfg());
}

/// Signed uplink↔downlink shift in Hz: TX (uplink) = sky_RX + shift. 0 = simplex.
export function txShiftHz(cfg) {
  const ex = cfg && cfg.backend_extras;
  const s = ex ? Number(ex.tx_rx_shift_hz) : 0;
  return Number.isFinite(s) ? s : 0;
}

/// Recompute the derived uplink into the active config. The Radio tab is the
/// SINGLE source of the TX frequency: TX = sky_RX + shift. Terrestrial simplex
/// is just shift = 0 (TX = RX); a repeater/cross-band split is a non-zero shift.
/// (The standalone Settings TX-frequency field is removed — see renderSdrPanel.)
export function deriveUplink(cfg, skyHz) {
  if (!cfg) return;
  const tx = skyHz + txShiftHz(cfg);
  if (Number.isFinite(tx) && tx >= 0) cfg.tx_freq_hz = Math.round(tx);
}

/// Update the read-only "TX frequency" display from a sky RX frequency.
export function updateTxFreqDisplay(skyHz, cfg) {
  const el = document.getElementById("radio-tx-freq-display");
  if (!el) return;
  if (!Number.isFinite(skyHz)) {
    el.textContent = "—";
    return;
  }
  const tx = skyHz + txShiftHz(cfg);
  el.textContent = tx >= 0 ? `${(tx / 1e6).toFixed(6)} MHz` : "—";
}

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
  const offset = currentSkyOffsetHz();
  if (t && disp) {
    // Big dial = the real sky frequency (IF + LNB LO when the LNB is enabled).
    disp.textContent = `${((t.displayed_rf_hz + offset) / 1e6).toFixed(6)} MHz`;
  }
  if (t && info) {
    const off = t.digital_offset_hz;
    const offStr = `${off >= 0 ? "+" : ""}${(off / 1000).toFixed(2)} kHz`;
    // The "OL" line stays the real hardware LO (the LNB IF when an LNB is in
    // use), tagged "(IF)" so it isn't mistaken for the dialed sky frequency.
    const loStr = offset !== 0
      ? `OL ${(t.lo_hz / 1e6).toFixed(4)} MHz (IF)`
      : `OL ${(t.lo_hz / 1e6).toFixed(4)} MHz`;
    info.textContent = `${loStr} · décalage num. ${offStr} · span ${(t.input_rate_hz / 1000).toFixed(0)} kHz`;
  }
  if (t) updateTxFreqDisplay(t.displayed_rf_hz + offset, activeRadioCfg());
}

export function currentRadioHz() {
  if (radioState.tune) return radioState.tune.displayed_rf_hz + currentSkyOffsetHz();
  // The input field already holds the sky frequency.
  const v = parseFloat(document.getElementById("radio-freq-input")?.value);
  return isFinite(v) ? Math.round(v * 1e6) : 145_500_000;
}

// `skyHz` is the real (sky) frequency the operator dials. The SDR tunes the IF
// = sky − LNB LO; both the dialed sky value and the derived uplink are kept in
// the active backend config.
export function tuneRadioTo(skyHz) {
  const sky = Math.max(0, Math.round(skyHz));
  const cfg = activeRadioCfg();
  const offset = skyOffsetHz(cfg);
  const ifHz = Math.max(0, Math.round(sky - offset)); // what the hardware tunes
  const input = document.getElementById("radio-freq-input");
  if (input) input.value = (sky / 1e6).toFixed(6); // operator sees the sky freq
  invokeRadio("set_radio_freq", { hz: ifHz });
  // Persist the dialed RF (IF) into the active RX backend's config so a later
  // plain "Start RX" (which reads the saved SDR config) reuses the last
  // frequency tuned here — the Settings RX panel no longer carries this field.
  if (cfg) {
    cfg.rx_freq_hz = ifHz;
    deriveUplink(cfg, sky); // TX = sky + shift (satellite / cross-band only)
    emit("settings:persist");
  }
  updateTxFreqDisplay(sky, cfg);
}

// One-click QO-100 narrowband transponder setup: enable the LNB display offset,
// set the standard uplink shift, switch to SSB-USB, show the band-plan overlay,
// and tune the sky centre (10489.750 MHz, the middle BPSK beacon).
export async function goToQo100() {
  const cfg = activeRadioCfg();
  if (!cfg) return;
  cfg.backend_extras = cfg.backend_extras || {};
  cfg.backend_extras.lnb_enabled = true;
  // Keep a previously-entered LO; otherwise seed the standard universal LNB.
  const lo = Number(cfg.backend_extras.lnb_lo_hz);
  if (!Number.isFinite(lo) || lo <= 0) cfg.backend_extras.lnb_lo_hz = QO100_LNB_LO_HZ;
  cfg.backend_extras.tx_rx_shift_hz = QO100_SHIFT_HZ;
  // Widen the analog RF filter so the whole ~500 kHz transponder (both edge
  // beacons) is passed — the 200 kHz NBFM default hides everything past ±100 kHz.
  // rf_bandwidth is an open-time parameter, hence the restart below.
  cfg.rf_bandwidth_hz = QO100_RF_BANDWIDTH_HZ;
  // SSB-USB demod (the transponder is linear; FM is meaningless here).
  cfg.rx_demod_mode = "ssb_usb";
  radioState.demodMode = "ssb_usb";
  const modeSel = document.getElementById("radio-demod-mode");
  if (modeSel) modeSel.value = "ssb_usb";
  invokeRadio("set_demod_mode", { mode: "ssb_usb" });
  applyDemodModeUi("ssb_usb");
  const bw = Number(document.getElementById("radio-ssb-bw")?.value) ||
    Number(cfg.ssb_bandwidth_hz) || 2700;
  invokeRadio("set_ssb_bandwidth", { hz: bw });
  radioState.bandPlanQo100 = true;
  // Reflect the satellite controls, then tune the centre (also derives + persists
  // the uplink and the IF the SDR hardware actually receives).
  seedSatControls(cfg);
  tuneRadioTo(QO100_DOWNLINK_CENTER_HZ);
  // rf_bandwidth only takes effect on (re)open, and build_capture_session
  // reloads the SdrConfig from settings.json — so flush the config to disk
  // BEFORE restarting, otherwise the new capture reopens with the old 200 kHz.
  if (isCapturing()) {
    await saveSettingsNow();
    restartRadioCapture().catch((e) => console.error("[radio] qo100 restart", e));
  }
}

/// Await a synchronous flush of the in-memory settings to disk (settings.json),
/// so a capture (re)start that reloads the config from disk sees the latest
/// values. The bus `settings:persist` is fire-and-forget and would race a restart.
export async function saveSettingsNow() {
  try {
    await invoke("save_settings", { settings: currentSettings });
  } catch (e) {
    console.error("[radio] save_settings", e);
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

  // LNB local oscillator (enable + LO MHz). Pure display offset: the hardware
  // does NOT retune — we only relabel IF↔sky, re-seed the dialed value and
  // refresh the read-outs. The freq scale / band-plan overlay redraw on the RAF.
  const onLnbChange = () => {
    const cfg = activeRadioCfg();
    if (!cfg) return;
    cfg.backend_extras = cfg.backend_extras || {};
    const en = document.getElementById("radio-lnb-enable");
    cfg.backend_extras.lnb_enabled = !!(en && en.checked);
    const loMhz = parseFloat(document.getElementById("radio-lnb-lo")?.value);
    if (Number.isFinite(loMhz)) cfg.backend_extras.lnb_lo_hz = Math.round(loMhz * 1e6);
    if (!cfg.backend_extras.lnb_enabled) radioState.bandPlanQo100 = false;
    reseedRadioSkyInput(cfg);
    updateRadioTuneDisplay();
    // The sky frequency for the fixed IF changed → recompute the derived uplink.
    deriveUplink(cfg, currentRadioHz());
    updateTxFreqDisplay(currentRadioHz(), cfg);
    emit("settings:persist");
  };
  document.getElementById("radio-lnb-enable")?.addEventListener("change", onLnbChange);
  document.getElementById("radio-lnb-lo")?.addEventListener("change", onLnbChange);

  // TX/RX shift (MHz, signed): TX uplink = sky_RX + shift. Recompute + persist
  // the derived uplink for the next TX.
  document.getElementById("radio-tx-shift")?.addEventListener("change", (e) => {
    const cfg = activeRadioCfg();
    if (!cfg) return;
    cfg.backend_extras = cfg.backend_extras || {};
    const mhz = parseFloat(e.target.value);
    cfg.backend_extras.tx_rx_shift_hz = Number.isFinite(mhz) ? Math.round(mhz * 1e6) : 0;
    const sky = currentRadioHz();
    deriveUplink(cfg, sky);
    updateTxFreqDisplay(sky, cfg);
    emit("settings:persist");
  });

  // TX RF power = Pluto TX attenuation (0 dB = full output). Persisted to the
  // backend config; applies on the next TX. While a tune carrier is on, rides
  // the power live (set_tune_power) so the operator can level on the downlink.
  const txAtt = document.getElementById("radio-tx-att");
  const txAttLabel = document.getElementById("radio-tx-att-label");
  txAtt?.addEventListener("input", () => {
    if (txAttLabel) txAttLabel.textContent = `${txAtt.value} dB`;
    const v = Number(txAtt.value); // value is attenuation dB (slider is rtl-flipped)
    if (!Number.isFinite(v)) return;
    if (radioState.tuning) {
      invoke("set_tune_power", { attenDb: v }).catch(() => {});
    } else {
      // Live-ride the AD9361 TX gain so an in-progress image/voice TX changes
      // immediately. Throttled — each call opens a brief control connection.
      const now = performance.now();
      if (now - (radioState.lastTxPowerSend || 0) >= 120) {
        radioState.lastTxPowerSend = now;
        const deviceName = document.getElementById("rx-device-select")?.value;
        if (deviceName) invoke("set_tx_power", { deviceName, attenDb: v }).catch(() => {});
      }
    }
  });
  txAtt?.addEventListener("change", () => {
    const cfg = activeRadioCfg();
    if (!cfg) return;
    cfg.backend_extras = cfg.backend_extras || {};
    let v = Number(txAtt.value);
    if (!Number.isFinite(v)) v = 30;
    v = Math.max(0, Math.min(89.75, v));
    cfg.backend_extras.tx_attenuation_db = v;
    if (txAttLabel) txAttLabel.textContent = `${v} dB`;
    // Apply the final value live too (the throttle may have dropped it), unless
    // a Tune is on (that path rides set_tune_power already).
    if (!radioState.tuning) {
      const deviceName = document.getElementById("rx-device-select")?.value;
      if (deviceName) invoke("set_tx_power", { deviceName, attenDb: v }).catch(() => {});
    }
    emit("settings:persist");
  });

  // One-click QO-100 NB transponder: LNB on, SSB-USB, shift, center + band plan.
  document.getElementById("radio-qo100")?.addEventListener("click", goToQo100);
  // Calibrate the LNB LO on the BPSK beacon (shown only near it, in SSB).
  document.getElementById("radio-beacon-cal")?.addEventListener("click", calibrateToBeacon);
  // CW tune carrier (TX) — toggle. Safety: stop it if the RX capture stops.
  document.getElementById("radio-tune-tx")?.addEventListener("click", () => {
    if (radioState.tuning) stopTuneTx();
    else startTuneTx();
  });
  on("capture:stopped", () => {
    if (radioState.tuning) stopTuneTx();
  });

  // Analog RF bandwidth (Pluto) — open-time parameter, so a change restarts the
  // capture when one is running. QO-100 needs ~540 kHz to pass the whole 500 kHz
  // transponder (the 200 kHz default hides everything past ±100 kHz).
  document.getElementById("radio-rfbw")?.addEventListener("change", async () => {
    const cfg = activeRadioCfg();
    if (!cfg) return;
    const khz = parseFloat(document.getElementById("radio-rfbw").value);
    if (!Number.isFinite(khz)) return;
    cfg.rf_bandwidth_hz = Math.round(khz * 1000);
    if (isCapturing()) {
      // Flush to disk before the restart reloads the config (see goToQo100).
      await saveSettingsNow();
      restartRadioCapture().catch((e) => console.error("[radio] rfbw restart", e));
    } else {
      emit("settings:persist");
    }
  });

  // Spectrum display: temporal smoothing (FFT averaging) + peak hang/decay.
  const sm = document.getElementById("radio-fft-smooth");
  const smLabel = document.getElementById("radio-fft-smooth-label");
  sm?.addEventListener("input", () => {
    if (smLabel) smLabel.textContent = `${sm.value} %`;
  });
  sm?.addEventListener("change", () => {
    ensureRadioSettings().fft_smooth_pct = Number(sm.value);
    emit("settings:persist");
  });
  document.getElementById("radio-hang-ms")?.addEventListener("change", (e) => {
    ensureRadioSettings().hang_ms = Math.max(0, Number(e.target.value) || 0);
    emit("settings:persist");
  });
  document.getElementById("radio-decay-dbs")?.addEventListener("change", (e) => {
    ensureRadioSettings().decay_db_s = Math.max(0, Number(e.target.value) || 0);
    emit("settings:persist");
  });

  // Level window: persist on commit + an Auto-fit to the current spectrum.
  const persistLevels = () => {
    const lmin = document.getElementById("radio-level-min");
    const lmax = document.getElementById("radio-level-max");
    const r = ensureRadioSettings();
    if (lmin) r.level_min_dbfs = Number(lmin.value);
    if (lmax) r.level_max_dbfs = Number(lmax.value);
    emit("settings:persist");
  };
  document.getElementById("radio-level-min")?.addEventListener("change", persistLevels);
  document.getElementById("radio-level-max")?.addEventListener("change", persistLevels);
  document.getElementById("radio-level-auto")?.addEventListener("click", autoFitLevels);

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
      emit("settings:persist");
    }
  });

  // Demod mode (NBFM ↔ SSB-USB) + SSB bandwidth. Applied live and persisted
  // into the active RX backend's config. The deviation control (NBFM) and the
  // SSB-bandwidth control are mutually exclusive in the UI.
  document.getElementById("radio-demod-mode")?.addEventListener("change", (e) => {
    const mode = e.target.value === "ssb_usb" ? "ssb_usb" : "nbfm";
    radioState.demodMode = mode;
    // The QO-100 band plan is an SSB-mode overlay; leaving SSB hides it.
    if (mode !== "ssb_usb") radioState.bandPlanQo100 = false;
    invokeRadio("set_demod_mode", { mode });
    applyDemodModeUi(mode);
    if (mode === "ssb_usb") {
      // Make sure the running chain matches the dropdown's bandwidth.
      const bw = Number(document.getElementById("radio-ssb-bw")?.value);
      if (Number.isFinite(bw) && bw > 0) invokeRadio("set_ssb_bandwidth", { hz: bw });
    }
    const backendId = getSelectedBackendId("rx-device-select");
    if (backendId) {
      const cfg = ensureBackendConfig(backendId);
      cfg.rx_demod_mode = mode;
      emit("settings:persist");
    }
  });
  document.getElementById("radio-ssb-bw")?.addEventListener("change", (e) => {
    const hz = Number(e.target.value);
    invokeRadio("set_ssb_bandwidth", { hz });
    const backendId = getSelectedBackendId("rx-device-select");
    if (backendId) {
      const cfg = ensureBackendConfig(backendId);
      cfg.ssb_bandwidth_hz = hz;
      emit("settings:persist");
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
    emit("settings:persist");
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
    emit("settings:persist");
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
  // Click-to-tune on the SSB spectrum-zoom panel: map the clicked x within the
  // ±ZOOM_HALF_HZ window (centred on the tuned point) to a sky frequency. SSB
  // only — in NBFM the same canvas shows the FM-excursion graph. 10 Hz step.
  const zoomCv = document.getElementById("radio-fm-excursion");
  zoomCv?.addEventListener("click", (e) => {
    if (radioState.demodMode !== "ssb_usb" || !radioState.tune) return;
    const rect = zoomCv.getBoundingClientRect();
    if (rect.width <= 0) return;
    const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    const sky =
      radioState.tune.displayed_rf_hz + (frac - 0.5) * 2 * ZOOM_HALF_HZ + currentSkyOffsetHz();
    tuneRadioTo(Math.round(sky / 10) * 10);
  });

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
      emit("settings:persist");
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
    emit("settings:persist");
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

  // Spectrum-display controls: smoothing, peak hang/decay, and the level window.
  const sm = document.getElementById("radio-fft-smooth");
  const smLabel = document.getElementById("radio-fft-smooth-label");
  if (sm) sm.value = String(r.fft_smooth_pct);
  if (smLabel) smLabel.textContent = `${r.fft_smooth_pct} %`;
  const hang = document.getElementById("radio-hang-ms");
  if (hang) hang.value = String(r.hang_ms);
  const decay = document.getElementById("radio-decay-dbs");
  if (decay) decay.value = String(r.decay_db_s);
  const lmin = document.getElementById("radio-level-min");
  const lmax = document.getElementById("radio-level-max");
  if (lmin) lmin.value = String(r.level_min_dbfs);
  if (lmax) lmax.value = String(r.level_max_dbfs);
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
  // Re-schedule unconditionally: a throw in any draw fn must NOT kill the
  // loop (otherwise the whole Radio tab freezes after one frame — the
  // `requestAnimationFrame` below would never run). Log and keep ticking.
  const loop = () => {
    try {
      renderRadio();
    } catch (err) {
      console.error("[radio] render error", err);
    }
    radioState.rafId = requestAnimationFrame(loop);
  };
  radioState.rafId = requestAnimationFrame(loop);
}

/// Show the deviation control in NBFM, the SSB-bandwidth control in SSB.
export function applyDemodModeUi(mode) {
  const ssb = mode === "ssb_usb";
  const devWrap = document.getElementById("radio-deviation-wrap");
  const bwWrap = document.getElementById("radio-ssb-bw-wrap");
  if (devWrap) devWrap.hidden = ssb;
  if (bwWrap) bwWrap.hidden = !ssb;
  // The right-hand panel is the FM-excursion meter (NBFM) or a zoom on the RF
  // spectrum around the tuned frequency (SSB), for fine RX tuning; retitle it.
  const title = document.getElementById("radio-meter-title");
  if (title) {
    title.textContent = ssb ? "Zoom spectre" : "Excursion FM";
    title.title = ssb
      ? "Zoom du spectre RF autour du point de réception (±15 kHz) — pour affiner l'accord"
      : "Déviation FM crête (avant dé-emphase) — rouge = surmodulation";
  }
  // The beacon-cal button is SSB-only; updateBeaconCalButton gates it on the
  // ±10 kHz proximity, but hide it outright when leaving SSB.
  if (!ssb) {
    const btn = document.getElementById("radio-beacon-cal");
    if (btn) btn.hidden = true;
  }
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
    // rx_freq_hz is the IF; show the sky frequency (IF + LNB LO when enabled).
    input.value = ((cfg.rx_freq_hz + skyOffsetHz(cfg)) / 1e6).toFixed(6);
  }
  // Channel width: reflect the persisted deviation (not session-driven).
  const dev = document.getElementById("radio-deviation");
  if (dev && Number.isFinite(cfg.max_deviation_hz)) {
    dev.value = String(Math.round(cfg.max_deviation_hz));
  }
  // Demod mode + SSB bandwidth: reflect the persisted backend config and set
  // the matching control visibility.
  const mode = cfg.rx_demod_mode === "ssb_usb" ? "ssb_usb" : "nbfm";
  radioState.demodMode = mode;
  const modeSel = document.getElementById("radio-demod-mode");
  if (modeSel) modeSel.value = mode;
  const bwSel = document.getElementById("radio-ssb-bw");
  if (bwSel && Number.isFinite(cfg.ssb_bandwidth_hz)) {
    bwSel.value = String(Math.round(cfg.ssb_bandwidth_hz));
  }
  applyDemodModeUi(mode);
  // LNB / TX-shift / TX-power controls reflect the persisted backend config.
  seedSatControls(cfg);
}

/// Reflect the persisted LNB / TX-shift / TX-power values into their controls
/// and refresh the derived TX-frequency read-out.
export function seedSatControls(cfg) {
  const ex = (cfg && cfg.backend_extras) || {};
  const en = document.getElementById("radio-lnb-enable");
  if (en) en.checked = !!ex.lnb_enabled;
  const lo = document.getElementById("radio-lnb-lo");
  if (lo) {
    const v = Number.isFinite(Number(ex.lnb_lo_hz)) ? Number(ex.lnb_lo_hz) : QO100_LNB_LO_HZ;
    lo.value = (v / 1e6).toFixed(3);
  }
  const sh = document.getElementById("radio-tx-shift");
  if (sh) {
    const v = Number.isFinite(Number(ex.tx_rx_shift_hz)) ? Number(ex.tx_rx_shift_hz) : 0;
    sh.value = (v / 1e6).toFixed(3);
  }
  const att = document.getElementById("radio-tx-att");
  const attLabel = document.getElementById("radio-tx-att-label");
  if (att) {
    const v = Number.isFinite(Number(ex.tx_attenuation_db)) ? Number(ex.tx_attenuation_db) : 30;
    att.value = String(v);
    if (attLabel) attLabel.textContent = `${v} dB`;
  }
  // RF bandwidth is a Pluto knob (SDRplay/RTL lock it) — show the row only there.
  const rfbwWrap = document.getElementById("radio-rfbw-wrap");
  const rfbw = document.getElementById("radio-rfbw");
  const isPluto = (cfg && cfg.backend_id === "pluto") ||
    getSelectedBackendId("rx-device-select") === "pluto";
  if (rfbwWrap) rfbwWrap.hidden = !isPluto;
  if (rfbw && Number.isFinite(Number(cfg.rf_bandwidth_hz))) {
    rfbw.value = String(Math.round(Number(cfg.rf_bandwidth_hz) / 1000));
  }
  updateTxFreqDisplay(currentRadioHz(), cfg);
}

/// Re-show the dialed sky frequency after the LNB offset changes (the hardware
/// does NOT move — only the IF↔sky relabel does).
export function reseedRadioSkyInput(cfg) {
  const input = document.getElementById("radio-freq-input");
  if (!input) return;
  const offset = skyOffsetHz(cfg);
  const ifHz = radioState.tune
    ? radioState.tune.displayed_rf_hz
    : (Number.isFinite(cfg.rx_freq_hz) ? cfg.rx_freq_hz : null);
  if (ifHz !== null && ifHz > 0) input.value = ((ifHz + offset) / 1e6).toFixed(6);
}

export function stopRadioRender() {
  if (radioState.rafId !== null) {
    cancelAnimationFrame(radioState.rafId);
    radioState.rafId = null;
  }
}

export function renderRadio() {
  drawRadioSmeter();
  // Audio spectrum cut at 4 kHz: NBFM useful audio is ~300-2700 Hz, so show
  // only 0-4 kHz of the 0-24 kHz band (= 4000/24000 of the bins) — the voice
  // band fills the panel and the dead high end is dropped.
  drawRadioSpectrum("radio-audio-fft", radioState.audio, "#29B6F6", 4000 / 24000);
  // The right-hand panel is mode-dependent: FM over-modulation excursion in
  // NBFM, a zoom on the RF spectrum around the tuned point in SSB (for fine
  // tuning — deviation is meaningless there).
  if (radioState.demodMode === "ssb_usb") drawSpectrumZoom();
  else drawFmExcursion();
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

// `binFrac` (0..1, default 1) renders only the lower fraction of the FFT
// bins across the full canvas width — used to show the audio spectrum at
// half-width (0-12 kHz of the 0-24 kHz band) where the NBFM voice band lives.
export function drawRadioSpectrum(canvasId, frame, color, binFrac = 1) {
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
  const n = Math.max(1, Math.floor(bins.length * Math.max(0, Math.min(1, binFrac))));
  const [lo, hi] = radioLevelRange();
  const span = hi - lo;
  ctx.strokeStyle = color;
  ctx.lineWidth = 1;
  ctx.beginPath();
  // Peak-preserving downsample: with many more bins than pixels (8192-bin RF
  // FFT), take the MAX bin per pixel column so narrow signals (beacons) survive.
  for (let x = 0; x < w; x++) {
    const b0 = Math.min(n - 1, Math.floor((x / w) * n));
    const b1 = Math.min(n, Math.max(b0 + 1, Math.floor(((x + 1) / w) * n)));
    let m = bins[b0];
    for (let b = b0 + 1; b < b1; b++) if (bins[b] > m) m = bins[b];
    const v = Math.max(0, Math.min(1, (m - lo) / span));
    const y = h - v * h;
    if (x === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

// Scrolling FM-excursion (over-modulation) graph. Plots the peak frequency
// deviation per frame as a strip-chart (newest at the right, scrolling left),
// with the RMS deviation overlaid as a line and reference grid lines at
// ±2.5 / ±5 kHz. The active max deviation is the over-modulation threshold:
// peaks above it are drawn red. Source = QuadratureDemod discriminator output
// tapped BEFORE de-emphasis (radio_fm_excursion telemetry), so it reflects the
// true on-air deviation. Magnitude (|deviation|) is shown — what matters for
// over-modulation is how far the peak swings, not its sign.
const EXC_HIST_MAX = 240; // ~12 s of history at the ~20 Hz emit cadence

export function drawFmExcursion() {
  const canvas = document.getElementById("radio-fm-excursion");
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  const dpr = window.devicePixelRatio || 1;
  const ex = radioState.excursion;
  const maxDev =
    ex && Number.isFinite(ex.max_dev_hz) && ex.max_dev_hz > 0 ? ex.max_dev_hz : 5000;
  // Ingest the latest frame once (dedup on seq vs the faster RAF). Each entry
  // is timestamped (performance.now, ms) so the 5 s peak-hold is time-accurate
  // whatever the emit cadence.
  if (ex && ex.seq !== radioState.excLastSeq) {
    radioState.excLastSeq = ex.seq;
    radioState.excHist.push({ peak: ex.peak_hz || 0, rms: ex.rms_hz || 0, t: performance.now() });
    if (radioState.excHist.length > EXC_HIST_MAX) {
      radioState.excHist.splice(0, radioState.excHist.length - EXC_HIST_MAX);
    }
  }
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = "#0a0a0a";
  ctx.fillRect(0, 0, w, h);
  const topHz = Math.max(6000, maxDev * 1.2);
  const yOf = (hz) => h - Math.max(0, Math.min(1, hz / topHz)) * h;

  // Reference grid: ±2.5 kHz (amateur narrow NBFM) and ±5 kHz (standard).
  // Labels DPR-scaled + right-aligned so they don't collide with the read-out.
  ctx.font = `${Math.round(9 * dpr)}px sans-serif`;
  ctx.textBaseline = "bottom";
  ctx.textAlign = "right";
  for (const r of [
    { hz: 2500, line: "rgba(120,200,120,0.45)", text: "#bfe6bf", label: "2,5 kHz" },
    { hz: 5000, line: "rgba(220,210,120,0.45)", text: "#e6dca0", label: "5 kHz" },
  ]) {
    if (r.hz >= topHz) continue;
    const y = yOf(r.hz);
    ctx.strokeStyle = r.line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
    ctx.fillStyle = r.text;
    ctx.fillText(r.label, w - 3 * dpr, y - 1);
  }
  ctx.textAlign = "left";
  // Over-modulation threshold = active max deviation (red dashed).
  const yMax = yOf(maxDev);
  ctx.strokeStyle = "rgba(244,67,54,0.85)";
  ctx.setLineDash([4, 3]);
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, yMax);
  ctx.lineTo(w, yMax);
  ctx.stroke();
  ctx.setLineDash([]);

  // Peak bars, newest at the right, scrolling left. Red above the threshold.
  const hist = radioState.excHist;
  const colW = w / EXC_HIST_MAX;
  for (let k = 0; k < hist.length; k++) {
    const s = hist[hist.length - 1 - k];
    const x = w - (k + 1) * colW;
    if (x + colW < 0) break;
    const yp = yOf(s.peak);
    ctx.fillStyle = s.peak > maxDev ? "#f44336" : "#43a047";
    ctx.fillRect(x, yp, Math.max(1, colW + 0.5), h - yp);
  }
  // RMS overlay as a thin line.
  if (hist.length > 1) {
    ctx.strokeStyle = "rgba(255,255,255,0.55)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let k = 0; k < hist.length; k++) {
      const s = hist[hist.length - 1 - k];
      const x = w - (k + 0.5) * colW;
      const y = yOf(s.rms);
      if (k === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }
  // Live read-out: current deviation + 5 s peak-hold, in a high-contrast box so
  // the numbers stay readable over the bars. DPR-scaled so the font isn't tiny
  // on HiDPI displays (the whole point of this read-out).
  if (hist.length) {
    const cur = hist[hist.length - 1].peak;
    const nowMs = performance.now();
    let pk2 = 0;
    for (let i = hist.length - 1; i >= 0; i--) {
      const e = hist[i];
      if (e.t !== undefined && nowMs - e.t > 2000) break;
      if (e.peak > pk2) pk2 = e.peak;
    }
    const fs = Math.round(13 * dpr);
    const pad = Math.round(4 * dpr);
    const lineH = Math.round(fs * 1.3);
    ctx.font = `bold ${fs}px sans-serif`;
    ctx.textBaseline = "top";
    ctx.textAlign = "left";
    const l1 = `cour ${(cur / 1000).toFixed(2)} kHz`;
    const l2 = `pic 2 s ${(pk2 / 1000).toFixed(2)} kHz`;
    const boxW =
      Math.max(ctx.measureText(l1).width, ctx.measureText(l2).width) + pad * 2;
    ctx.fillStyle = "rgba(0,0,0,0.6)";
    ctx.fillRect(0, 0, boxW, lineH * 2 + pad);
    ctx.fillStyle = cur > maxDev ? "#ff8a80" : "#e6ffe6";
    ctx.fillText(l1, pad, pad);
    ctx.fillStyle = pk2 > maxDev ? "#ff5252" : "#a8e6a8";
    ctx.fillText(l2, pad, pad + lineH);
  }
}

// Half-width of the SSB fine-tuning zoom around the tuned point.
export const ZOOM_HALF_HZ = 10000; // ±10 kHz

/// Show the QO-100 beacon-calibration button only in SSB and within ±10 kHz of
/// the BPSK beacon (10489.750 MHz sky).
export function updateBeaconCalButton() {
  const btn = document.getElementById("radio-beacon-cal");
  if (!btn) return;
  const ssb = radioState.demodMode === "ssb_usb";
  const near = Math.abs(currentRadioHz() - QO100_DOWNLINK_CENTER_HZ) <= 10_000;
  btn.hidden = !(ssb && near);
}

/// Calibrate the LNB LO on the QO-100 BPSK beacon. Assumes the operator centred
/// the zoom on the beacon: shift the LNB LO so the dialed point reads exactly
/// 10489.750 MHz, then recentre the LO on it (beacon at the spectrum centre).
export function calibrateToBeacon() {
  const cfg = activeRadioCfg();
  if (!cfg) return;
  cfg.backend_extras = cfg.backend_extras || {};
  const sky = currentRadioHz();
  const loOld = Number(cfg.backend_extras.lnb_lo_hz);
  const delta = QO100_DOWNLINK_CENTER_HZ - sky;
  cfg.backend_extras.lnb_lo_hz =
    Math.round((Number.isFinite(loOld) ? loOld : QO100_LNB_LO_HZ) + delta);
  tuneRadioTo(QO100_DOWNLINK_CENTER_HZ); // relabel the channel + persist
  invokeRadio("recenter_lo"); // centre the LO on the beacon
  seedSatControls(cfg);
  updateRadioTuneDisplay();
}

/// Reflect the TX-tune button state (idle vs transmitting).
export function setTuneBtnState(on) {
  const btn = document.getElementById("radio-tune-tx");
  if (!btn) return;
  btn.classList.toggle("tx-on", on);
  btn.textContent = on ? "■ TX" : "Tune";
}

/// Start the CW tune carrier (TX). Pluto only, full-duplex; auto-stops after
/// 30 s on both sides. Power follows the live "Puiss. TX" slider.
export function startTuneTx() {
  const deviceName = document.getElementById("rx-device-select")?.value;
  if (!deviceName) return;
  const attenDb = Number(document.getElementById("radio-tx-att")?.value);
  invoke("start_tune", { deviceName, attenDb: Number.isFinite(attenDb) ? attenDb : 30 })
    .then(() => {
      radioState.tuning = true;
      setTuneBtnState(true);
      if (radioState.tuneTimer) clearTimeout(radioState.tuneTimer);
      radioState.tuneTimer = setTimeout(() => stopTuneTx(), 30_000);
    })
    .catch((err) => {
      console.error("[tune] start", err);
      const st = document.getElementById("status");
      if (st) st.textContent = String(err);
    });
}

export function stopTuneTx() {
  invoke("stop_tune").catch(() => {});
  radioState.tuning = false;
  setTuneBtnState(false);
  if (radioState.tuneTimer) {
    clearTimeout(radioState.tuneTimer);
    radioState.tuneTimer = null;
  }
}

/// Zoom on the RF spectrum around the tuned point (±ZOOM_HALF_HZ) for fine SSB
/// tuning — replaces the SSB level meter. Centre marker = the tuned frequency,
/// ±5/±10 kHz grid. Reuses the smoothed RF frame and the level window.
export function drawSpectrumZoom() {
  const canvas = document.getElementById("radio-fm-excursion");
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  const dpr = window.devicePixelRatio || 1;
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = "#0a0a0a";
  ctx.fillRect(0, 0, w, h);
  updateBeaconCalButton();
  const frame = radioSmoothedRf();
  if (!frame || !frame.bins_db || !frame.bins_db.length || !frame.span_hz) return;
  const bins = frame.bins_db;
  const n = bins.length;
  const hzPerBin = frame.span_hz / n;
  const loEdge = frame.center_hz - frame.span_hz / 2;
  const tunedIf = radioState.tune ? radioState.tune.displayed_rf_hz : frame.center_hz;
  const centerBin = (tunedIf - loEdge) / hzPerBin;
  const halfBins = ZOOM_HALF_HZ / hzPerBin;
  const b0 = centerBin - halfBins;
  const binSpan = 2 * halfBins || 1;
  const [loDb, hiDb] = radioLevelRange();
  const dbSpan = (hiDb - loDb) || 1;
  // USB received passband: the SSB filter keeps the upper sideband, from the
  // tuned carrier up to +ssb_bw. Faint green band (drawn behind the trace) with
  // an edge line at +ssb_bw — shows exactly what's being received.
  const ssbBw = Number(document.getElementById("radio-ssb-bw")?.value) || 2700;
  const pbW = ((ssbBw / hzPerBin) / binSpan) * w;
  ctx.fillStyle = "rgba(80,200,120,0.10)";
  ctx.fillRect(w * 0.5, 0, Math.max(1, pbW), h);
  // ±kHz grid relative to the centre.
  ctx.font = `${Math.round(9 * dpr)}px sans-serif`;
  ctx.textAlign = "center";
  ctx.textBaseline = "bottom";
  for (const k of [-10, -5, 5, 10]) {
    const x = (w * ((k * 1000) / hzPerBin + halfBins)) / binSpan;
    if (x < 0 || x > w) continue;
    ctx.strokeStyle = "rgba(255,255,255,0.08)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, h);
    ctx.stroke();
    ctx.fillStyle = "#8a9099";
    ctx.fillText(`${k > 0 ? "+" : ""}${k}`, x, h - 1);
  }
  // Spectrum trace — peak-preserving per pixel (see drawRadioSpectrum).
  ctx.strokeStyle = "#29B6F6";
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let x = 0; x < w; x++) {
    const bi0 = Math.floor(b0 + (x / w) * binSpan);
    const bi1 = Math.max(bi0 + 1, Math.floor(b0 + ((x + 1) / w) * binSpan));
    let m = -Infinity;
    for (let b = bi0; b < bi1; b++) if (b >= 0 && b < n && bins[b] > m) m = bins[b];
    const v = m > -Infinity ? Math.max(0, Math.min(1, (m - loDb) / dbSpan)) : 0;
    const y = h - v * h;
    if (x === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
  // Passband upper edge line at +ssb_bw.
  ctx.strokeStyle = "rgba(80,200,120,0.6)";
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(w * 0.5 + pbW, 0);
  ctx.lineTo(w * 0.5 + pbW, h);
  ctx.stroke();
  // Centre line = the tuned frequency (the carrier / tuning reference) — bold.
  ctx.strokeStyle = "rgba(127,209,255,0.95)";
  ctx.lineWidth = Math.max(1.5, 1.5 * dpr);
  ctx.beginPath();
  ctx.moveTo(w * 0.5, 0);
  ctx.lineTo(w * 0.5, h);
  ctx.stroke();
}

export function drawSsbLevel() {
  const canvas = document.getElementById("radio-fm-excursion");
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  const dpr = window.devicePixelRatio || 1;
  const lv = radioState.audioLevel;
  // Ingest the latest frame once (dedup on seq vs the faster RAF).
  if (lv && lv.seq !== radioState.levelLastSeq) {
    radioState.levelLastSeq = lv.seq;
    radioState.levelHist.push({ peak: lv.peak || 0, rms: lv.rms || 0, t: performance.now() });
    if (radioState.levelHist.length > EXC_HIST_MAX) {
      radioState.levelHist.splice(0, radioState.levelHist.length - EXC_HIST_MAX);
    }
  }
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = "#0a0a0a";
  ctx.fillRect(0, 0, w, h);
  // Linear scale with a little headroom above full scale.
  const top = 1.2;
  const yOf = (v) => h - Math.max(0, Math.min(1, v / top)) * h;

  // Reference grid: -6 dB / -12 dB (linear 0.5 / 0.25) and the 0 dBFS clip line.
  ctx.font = `${Math.round(9 * dpr)}px sans-serif`;
  ctx.textBaseline = "bottom";
  ctx.textAlign = "right";
  for (const r of [
    { v: 0.25, line: "rgba(120,160,200,0.40)", text: "#a8c4e6", label: "-12 dB" },
    { v: 0.5, line: "rgba(120,200,120,0.40)", text: "#bfe6bf", label: "-6 dB" },
  ]) {
    const y = yOf(r.v);
    ctx.strokeStyle = r.line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
    ctx.fillStyle = r.text;
    ctx.fillText(r.label, w - 3 * dpr, y - 1);
  }
  ctx.textAlign = "left";
  // 0 dBFS clip line (red dashed).
  const yClip = yOf(1.0);
  ctx.strokeStyle = "rgba(244,67,54,0.85)";
  ctx.setLineDash([4, 3]);
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, yClip);
  ctx.lineTo(w, yClip);
  ctx.stroke();
  ctx.setLineDash([]);

  // Peak bars, newest at the right, scrolling left. Red at/above full scale.
  const hist = radioState.levelHist;
  const colW = w / EXC_HIST_MAX;
  for (let k = 0; k < hist.length; k++) {
    const s = hist[hist.length - 1 - k];
    const x = w - (k + 1) * colW;
    if (x + colW < 0) break;
    const yp = yOf(s.peak);
    ctx.fillStyle = s.peak >= 1.0 ? "#f44336" : "#26a69a";
    ctx.fillRect(x, yp, Math.max(1, colW + 0.5), h - yp);
  }
  // RMS overlay line.
  if (hist.length > 1) {
    ctx.strokeStyle = "rgba(255,255,255,0.55)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let k = 0; k < hist.length; k++) {
      const s = hist[hist.length - 1 - k];
      const x = w - (k + 0.5) * colW;
      const y = yOf(s.rms);
      if (k === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }
  // Read-out in dBFS (peak + RMS) over a high-contrast box.
  if (hist.length) {
    const last = hist[hist.length - 1];
    const dbfs = (v) => (v > 1e-4 ? 20 * Math.log10(v) : -80);
    const fs = Math.round(13 * dpr);
    const pad = Math.round(4 * dpr);
    const lineH = Math.round(fs * 1.3);
    ctx.font = `bold ${fs}px sans-serif`;
    ctx.textBaseline = "top";
    ctx.textAlign = "left";
    const l1 = `crête ${dbfs(last.peak).toFixed(1)} dBFS`;
    const l2 = `RMS ${dbfs(last.rms).toFixed(1)} dBFS`;
    const boxW =
      Math.max(ctx.measureText(l1).width, ctx.measureText(l2).width) + pad * 2;
    ctx.fillStyle = "rgba(0,0,0,0.6)";
    ctx.fillRect(0, 0, boxW, lineH * 2 + pad);
    ctx.fillStyle = last.peak >= 1.0 ? "#ff5252" : "#b2f0ea";
    ctx.fillText(l1, pad, pad);
    ctx.fillStyle = "#9be29b";
    ctx.fillText(l2, pad, pad + lineH);
  }
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
  // Tick positions stay in the IF domain (the spectrum is LO-centred); only the
  // printed labels carry the LNB offset so they read as sky frequencies.
  const skyOffset = currentSkyOffsetHz();
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
    const skyF = f + skyOffset;
    const lbl =
      i === ticks.length - 1 ? `${(skyF / 1e6).toFixed(3)} MHz` : (skyF / 1e6).toFixed(3);
    ctx.fillText(lbl, x, midY + 1.5 * dpr);
  }
}

// dB graticule on the RF line spectrum: faint horizontal lines + dBFS labels
// at "nice" steps across the operator-set level window (radioLevelRange). The
// vertical Hz grid is drawn separately (drawRfGridlines / drawRadioFreqScale).
// Drawn AFTER the trace so the labels stay readable; lines are faint enough not
// to fight the spectrum.
export function drawRfDbGraticule(canvasId) {
  const canvas = document.getElementById(canvasId);
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  const [lo, hi] = radioLevelRange();
  const span = hi - lo || 1;
  const dpr = window.devicePixelRatio || 1;
  const step = niceTickStep(span, 5); // ~5 divisions
  ctx.font = `${Math.round(9 * dpr)}px sans-serif`;
  ctx.textBaseline = "bottom";
  ctx.textAlign = "left";
  ctx.lineWidth = 1;
  const top = Math.floor(hi / step) * step;
  for (let db = top; db >= lo; db -= step) {
    const y = h - ((db - lo) / span) * h;
    if (y < 1 || y > h - 1) continue;
    ctx.strokeStyle = "rgba(125,135,148,0.22)";
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
    ctx.fillStyle = "#8a9099";
    // Unit only on the top label to keep the column uncluttered.
    const lbl = db === top ? `${db} dBFS` : `${db}`;
    ctx.fillText(lbl, 3 * dpr, y - 1.5 * dpr);
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
  // radioFreqAtX returns the IF-domain frequency (the spectrum is centred on the
  // hardware LO); add the LNB offset so we hand tuneRadioTo a sky frequency.
  const offset = currentSkyOffsetHz();
  // In SSB you tune to the suppressed carrier (the signal sits ABOVE it as
  // USB), so the strongest RF bin is ~1 kHz off the carrier — peak-snap would
  // mistune. Honor the clicked frequency instead, rounded to the 100 Hz.
  if (radioState.demodMode === "ssb_usb") {
    return Math.round(clickHz / 100) * 100 + offset;
  }
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
      return Math.round(hz / 100) * 100 + offset;
    }
  }
  // Fallback: tune exactly where the user clicked, rounded to the kHz.
  return Math.round(clickHz / 1000) * 1000 + offset;
}

// QO-100 NB transponder band-plan overlay, drawn on the RF spectrum when the
// QO-100 preset is active. Beacons are the firm anchors (lower/upper CW edges +
// central BPSK); the CW/SSB/digital segment shading reflects the conventional
// AMSAT-DL usage zones (indicative). Markers are sky frequencies, mapped to the
// IF-domain pixel axis via `ifHz = sky − skyOffset` so they track the tuning and
// the LNB offset; anything off-screen is clipped.
export const QO100_BAND_MARKERS = [
  { hz: QO100_BEACON_LOWER_HZ, label: "Bal. CW", color: "#ff7043" },
  { hz: QO100_DOWNLINK_CENTER_HZ, label: "Bal. BPSK", color: "#7fd1ff" },
  { hz: QO100_BEACON_UPPER_HZ, label: "Bal. CW", color: "#ff7043" },
];

export const QO100_BAND_SEGMENTS = [
  { lo: 10_489_500_000, hi: 10_489_550_000, color: "rgba(255,112,67,0.10)" }, // CW
  { lo: 10_489_550_000, hi: 10_489_990_000, color: "rgba(120,200,120,0.07)" }, // SSB
  { lo: 10_489_990_000, hi: 10_490_000_000, color: "rgba(120,160,200,0.12)" }, // digital
];

export function drawQo100BandPlan(canvasId) {
  if (!radioState.bandPlanQo100) return;
  const canvas = document.getElementById(canvasId);
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const frame = radioState.rf;
  if (!frame || !frame.span_hz) return;
  const w = canvas.width;
  const h = canvas.height;
  const dpr = window.devicePixelRatio || 1;
  const offset = currentSkyOffsetHz();
  const loEdge = frame.center_hz - frame.span_hz / 2;
  const xOf = (skyHz) => ((skyHz - offset - loEdge) / frame.span_hz) * w;
  // Faint usage-segment bands.
  for (const s of QO100_BAND_SEGMENTS) {
    let x0 = xOf(s.lo);
    let x1 = xOf(s.hi);
    if (x1 < 0 || x0 > w) continue;
    x0 = Math.max(0, x0);
    x1 = Math.min(w, x1);
    ctx.fillStyle = s.color;
    ctx.fillRect(x0, 0, Math.max(1, x1 - x0), h);
  }
  // Beacon markers (dashed verticals) + labels at the top.
  ctx.font = `${Math.round(9 * dpr)}px sans-serif`;
  ctx.textBaseline = "top";
  for (const m of QO100_BAND_MARKERS) {
    const x = xOf(m.hz);
    if (x < 0 || x > w) continue;
    ctx.strokeStyle = m.color;
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, h);
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.fillStyle = m.color;
    ctx.textAlign = x > w * 0.85 ? "right" : x < w * 0.15 ? "left" : "center";
    ctx.fillText(m.label, Math.max(2, Math.min(w - 2, x)), 2 * dpr);
  }
  // Small caption so the overlay reads as the band plan (UI string: French).
  ctx.fillStyle = "rgba(180,200,255,0.85)";
  ctx.textAlign = "left";
  ctx.textBaseline = "bottom";
  ctx.fillText("Plan QO-100 (bande étroite)", 3 * dpr, h - 2 * dpr);
}

// Blend factor for the new RF frame in the temporal average. The "Lissage"
// slider is a smoothing *strength* (0 % = off → α 1, raw); higher = heavier
// averaging (smaller α). Floored so the display still tracks the band.
export function radioFftSmoothAlpha() {
  const pct = parseFloat(document.getElementById("radio-fft-smooth")?.value ?? "0");
  return Math.max(0.05, 1 - Math.max(0, Math.min(95, pct)) / 100);
}

// Exponentially-averaged copy of the latest RF frame (dB-domain EMA per bin).
// Returns the raw frame unchanged when smoothing is off or before the first
// frame. The average only folds in a genuinely new frame (dedup on seq), so the
// RAF cadence doesn't bias it.
export function radioSmoothedRf() {
  const f = radioState.rf;
  if (!f || !f.bins_db || !f.bins_db.length) return f;
  const pct = parseFloat(document.getElementById("radio-fft-smooth")?.value ?? "0");
  if (!(pct > 0)) return f;
  const n = f.bins_db.length;
  let sm = radioState.rfSmoothed;
  if (!sm || !sm.bins_db || sm.bins_db.length !== n) {
    sm = { bins_db: Float32Array.from(f.bins_db), center_hz: f.center_hz, span_hz: f.span_hz, seq: f.seq };
    radioState.rfSmoothed = sm;
    radioState.rfSmoothSeq = f.seq;
    return sm;
  }
  if (f.seq !== radioState.rfSmoothSeq) {
    radioState.rfSmoothSeq = f.seq;
    const a = radioFftSmoothAlpha();
    const b = f.bins_db;
    const s = sm.bins_db;
    for (let i = 0; i < n; i++) s[i] += a * (b[i] - s[i]);
    sm.center_hz = f.center_hz;
    sm.span_hz = f.span_hz;
    sm.seq = f.seq;
  }
  return sm;
}

export function radioHangMs() {
  const v = parseFloat(document.getElementById("radio-hang-ms")?.value ?? "225");
  return Number.isFinite(v) && v >= 0 ? v : 225;
}

export function radioDecayDbS() {
  const v = parseFloat(document.getElementById("radio-decay-dbs")?.value ?? "55");
  return Number.isFinite(v) && v >= 0 ? v : 55;
}

// Peak hang/decay envelope of the RF line spectrum, fed by the smoothed frame.
// Per bin: instant attack to the input, hold for `hang` ms, then fall at
// `decay` dB/s (never below the live input). Runs every RAF on real elapsed
// time so the decay is frame-rate independent. decay = 0 disables it (returns
// the smoothed frame). Classic SDR peak display — surfaces weak / fleeting
// signals that a single FFT snapshot drops into the noise.
export function radioDisplayRf() {
  const smf = radioSmoothedRf();
  if (!smf || !smf.bins_db || !smf.bins_db.length) return smf;
  const decayDbS = radioDecayDbS();
  if (!(decayDbS > 0)) {
    radioState.rfHeld = null;
    return smf;
  }
  const n = smf.bins_db.length;
  const now = performance.now();
  const hangMs = radioHangMs();
  let held = radioState.rfHeld;
  if (!held || !held.bins_db || held.bins_db.length !== n) {
    held = {
      bins_db: Float32Array.from(smf.bins_db),
      center_hz: smf.center_hz,
      span_hz: smf.span_hz,
      seq: smf.seq,
      hangUntil: new Float64Array(n).fill(now + hangMs),
    };
    radioState.rfHeld = held;
    radioState.rfDisplayLastMs = now;
    return held;
  }
  const dt = Math.max(0, (now - (radioState.rfDisplayLastMs || now)) / 1000);
  radioState.rfDisplayLastMs = now;
  const drop = decayDbS * dt; // dB to fall this tick
  const inp = smf.bins_db;
  const h = held.bins_db;
  const hu = held.hangUntil;
  for (let i = 0; i < n; i++) {
    const x = inp[i];
    if (x >= h[i]) {
      h[i] = x; // attack: instant rise
      hu[i] = now + hangMs;
    } else if (now >= hu[i]) {
      h[i] = Math.max(x, h[i] - drop); // decay, floored at the live input
    } // else: hang (hold)
  }
  held.center_hz = smf.center_hz;
  held.span_hz = smf.span_hz;
  held.seq = smf.seq;
  return held;
}

// Fit the level window to the current spectrum: park the noise floor (robust
// ~20th-percentile bin) near the bottom and open a ~40 dB window above it
// (QO-100 SNR tops out ≈ 35 dB), so weak level differences fill the scale.
export function autoFitLevels() {
  const f = radioState.rf;
  if (!f || !f.bins_db || !f.bins_db.length) return;
  const sorted = Float64Array.from(f.bins_db).sort();
  const floor = sorted[Math.floor(sorted.length * 0.20)];
  const lmin = document.getElementById("radio-level-min");
  const lmax = document.getElementById("radio-level-max");
  const clampTo = (el, v) => Math.max(Number(el.min), Math.min(Number(el.max), v));
  if (lmin) lmin.value = String(Math.round(clampTo(lmin, floor - 3)));
  if (lmax) lmax.value = String(Math.round(clampTo(lmax, floor + 37)));
  const r = ensureRadioSettings();
  if (lmin) r.level_min_dbfs = Number(lmin.value);
  if (lmax) r.level_max_dbfs = Number(lmax.value);
  emit("settings:persist");
}

// Faint vertical frequency gridlines over a fully-repainted canvas (RF line
// spectrum). Not used on the waterfall, whose scroll buffer would smear them.
export function drawRfGridlines(canvasId, frame) {
  if (!frame || !frame.span_hz) return;
  const canvas = document.getElementById(canvasId);
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const w = canvas.width;
  const h = canvas.height;
  const { lo, hi, ticks } = radioRfTicks(frame);
  const span = hi - lo || 1;
  for (const f of ticks) {
    const x = ((f - lo) / span) * w;
    const isCenter = Math.abs(f - frame.center_hz) < span * 0.01;
    ctx.strokeStyle = isCenter ? "rgba(127,209,255,0.35)" : "rgba(255,255,255,0.07)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, h);
    ctx.stroke();
  }
}

// Thin vertical line at the tuned frequency, over a fully-repainted canvas (the
// RF spectrum). The waterfall gets its own scrolling marker column below.
export function drawTunedMarker(canvasId) {
  const canvas = document.getElementById(canvasId);
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const x = Math.round(canvas.width * radioMarkerFrac()) + 0.5;
  ctx.strokeStyle = "rgba(127,209,255,0.85)";
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(x, 0);
  ctx.lineTo(x, canvas.height);
  ctx.stroke();
}

export function drawRadioRf() {
  // Top: line spectrum (with frequency gridlines). Middle: shared Hz
  // ruler. Bottom: scrolling waterfall — all three share the same
  // horizontal Hz mapping from the latest RF frame. The line spectrum uses the
  // peak hang/decay envelope (over the smoothed frame); the waterfall uses the
  // smoothed frame (time is its own axis). Geometry overlays read identical
  // center/span from either.
  const wfFrame = radioSmoothedRf();
  const lineFrame = radioDisplayRf();
  drawRadioSpectrum("radio-rf-fft", lineFrame, "#9CCC65");
  drawRfDbGraticule("radio-rf-fft");
  drawRfGridlines("radio-rf-fft", lineFrame);
  drawTunedMarker("radio-rf-fft");
  drawQo100BandPlan("radio-rf-fft");
  drawRadioFreqScale();
  const canvas = document.getElementById("radio-waterfall");
  const ctx = sizeRadioCanvas(canvas);
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  // Re-init only when the WATERFALL canvas geometry itself changes (or on the
  // first paint / tab open). Decoupled from the other canvases: sizing the
  // smeter / audio / RF / excursion canvases must NOT wipe the accumulated
  // waterfall history — that coupling is what stopped the waterfall scrolling.
  if (!radioState.waterfallInit || w !== radioState.wfW || h !== radioState.wfH) {
    ctx.fillStyle = "#000";
    ctx.fillRect(0, 0, w, h);
    radioState.waterfallInit = true;
    radioState.wfW = w;
    radioState.wfH = h;
    radioState.lastWfSeq = -1;
  }
  const frame = wfFrame;
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
    // Peak-preserving downsample (max bin per pixel column) — see drawRadioSpectrum.
    const b0 = Math.min(n - 1, Math.floor((x / w) * n));
    const b1 = Math.min(n, Math.max(b0 + 1, Math.floor(((x + 1) / w) * n)));
    let m = bins[b0];
    for (let b = b0 + 1; b < b1; b++) if (bins[b] > m) m = bins[b];
    const v = Math.max(0, Math.min(1, (m - lo) / span));
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
