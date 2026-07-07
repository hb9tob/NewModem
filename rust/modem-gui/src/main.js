// NBFM Modem GUI — 3-tab layout (RX / TX / Info) with per-block progress and
// live constellation display.

import { initI18n, setLang, getLang, supportedLangs, t, applyI18n } from "./i18n.js";
import { MIME_TYPES, MIME_BINARY, MIME_TEXT, MIME_IMAGE_AVIF, MIME_IMAGE_JPEG, MIME_IMAGE_PNG, MIME_ZSTD, mimeToExt, isImageMime, now, escapeHtml, numOr, fmtSeconds, txFormatBytes, IMAGE_EXTS, isImageFilename, formatTimestamp, formatBytes, fmtNumOrDash } from "./lib/format.js";
import { invoke, listen, convertFileSrc, getCurrentWindow, openExternalUrl } from "./lib/ipc.js";
import { getSelectedBackendId, makeRow, makeFieldLabel, rxIsRunning } from "./lib/dom.js";
import { eventLogBuffer, logEvent } from "./lib/log.js";
import { currentSettings, setSettings, modemProfiles, setModemProfiles } from "./lib/state.js";
import { openLightbox, setupLightbox } from "./lib/lightbox.js";
import { sdrBackends, EIA_CTCSS_TONES_HZ, loadSdrBackends, isBackendEnabled, renderSdrBackendsList, resolveDeviceCaps, prefetchCapsForSelected, getCapsForSelected, ensureBackendConfig, renderSdrPanel, refreshSdrPanels, hasFeatureToggles, buildAgcRow, buildGainRow, buildAntennaRow, buildFeatureRow, buildBackendExtrasRow, onSdrFieldChange, isFreqInputId, backendIdForFreqInput, freqFavoritesArray, pushFreqMru } from "./lib/sdr.js";
import { on as onBus } from "./lib/bus.js";
import { setupSelectPicker, setupVirtKeyboard } from "./lib/kiosk.js";
import { startCapture, tryAutoStartCapture, startCaptureFromWav, stopCapture } from "./lib/capture.js";
import { refreshSessions, upsertSession, renderSessionsTable } from "./tabs/sessions.js";
import { setupChannelTab } from "./tabs/channel.js";
import { setupSounderTab } from "./tabs/sounder.js";
import { ensureRadioSettings, radioState, refreshRadioTabVisibility, updateRadioTuneDisplay, tuneRadioTo, setupRadioTab, setupRadioSdrModal, startRadioRender, stopRadioRender } from "./tabs/radio.js";
import { refreshSettingsRxWarn, loadDevices, applyTurboModeStyling, applyRxForceSettingsToUI, loadModemProfiles, applyExperimentalModesToUI, applyPttSettingsToUI, loadSerialPorts, renderPttStatus, persistSettings, setupSettingsTab, loadSaveDir } from "./tabs/settings.js";
import { makeDefaultOverlays, ensureOverlaySlots, getActiveOverlayPayload, setupOverlaysTab, applyOverlaysToUI } from "./tabs/overlays.js";
import { setupHistoryTab, refreshHistory } from "./tabs/history.js";
import { OVD_STICKY_MS, lastProgress, lastPilotPhases, fountainState, showCurrentFile, revealReceivedFile, refreshRxDeviceLabel, refreshStartButtonFromRx, refreshRawRecordingState, toggleRawRecording, submitPendingCapture, dismissCapturePrompt, updateLevel, refreshOverdriveChip, noteAudioOverdrive, noteRxRealtime, noteRxRealtimeReset, noteProfileFromHeader, updateV2State, resetRxVisuals, hideFountainStatus, updateFountainStatus, updateV2Progress, redrawAll, drawProgressBlocks, drawPilotPhase, setProgressBitmap } from "./tabs/rx.js";
import { txState, refreshTxExperimentalWarn, scheduleTxCompress, refreshTxEstimate, setupTxTab, txStop, refreshDuplexTxBar, onTxProgress, onTxComplete, onTxError, relayHistoryItem, resumeTxFromHistory } from "./tabs/tx.js";

// ────────────────────────────────────────────────────────── Language
// The <select id="lang-select"> in the Settings panel persists across
// reloads (i18n.js localStorage). On change we let i18n.js rewalk
// the DOM via applyI18n + fire `langchange`; everything dynamic
// (sessions table, history list, status chips, etc.) re-renders
// itself by listening on that event.
function setupLangSelect() {
  const sel = document.getElementById("lang-select");
  if (!sel) return;
  // Pre-select the active language and seed the visible options
  // from supportedLangs() so adding a 3rd language is a JSON drop.
  sel.innerHTML = "";
  for (const lang of supportedLangs()) {
    const opt = document.createElement("option");
    opt.value = lang;
    opt.setAttribute("data-i18n", `lang.${lang}`);
    opt.textContent = t(`lang.${lang}`) || lang.toUpperCase();
    sel.appendChild(opt);
  }
  sel.value = getLang();
  sel.addEventListener("change", async () => {
    try { await setLang(sel.value); } catch (err) { console.error("setLang", err); }
  });
  document.addEventListener("langchange", () => {
    // Keep the visible value in sync if setLang was invoked programmatically.
    sel.value = getLang();
    // Re-render every list/table that builds its own DOM and would
    // otherwise still show the previous language. Wrapped because
    // some renderers run before their backing state has loaded.
    try { renderSessionsTable(); } catch (_) {}
    try { refreshHistory(); } catch (_) {}
    try { renderSdrBackendsList(); } catch (_) {}
  });
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
      if (target === "rx") {
        redrawAll();
        tryAutoStartCapture().catch((err) => console.error("auto-start RX", err));
      }
      if (target === "sessions") refreshSessions();
      if (target === "history") refreshHistory();
      if (target === "channel") stopRxAndTxForChannelTab();
      if (target === "settings") refreshSettingsRxWarn();
      if (target === "radio") startRadioRender();
      else stopRadioRender();
    });
  }
}

// Channel tab: we stop TX in progress on entry (the attenuation setting
// applies to the next TX, and a running TX would interfere with sounding).
// RX is stopped too for sound-card sources — the standalone sounder capture
// needs the audio device free. But an SDR receiver is KEPT running: the
// sounder taps its live audio (raw-capture tee, see runSounderRxCaptureToggle)
// instead of opening the device a second time, so the operator can sound the
// channel with the SDR without losing reception.
async function stopRxAndTxForChannelTab() {
  const stopBtn = document.getElementById("btn-stop");
  const txStopBtn = document.getElementById("tx-btn-stop");
  const rxRunning = stopBtn && !stopBtn.disabled;
  const txRunning = txStopBtn && !txStopBtn.disabled;
  const rxIsSdr = getSelectedBackendId("rx-device-select") !== null;
  if (rxRunning && !rxIsSdr) {
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

async function loadSettings() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    setSettings(await invoke("get_settings"));
  } catch (err) {
    console.error("get_settings", err);
    setSettings({
      callsign: "", rx_device: "", tx_device: "",
      ptt_enabled: false, ptt_port: "",
      ptt_use_rts: true, ptt_use_dtr: false,
      ptt_rts_tx_high: true, ptt_dtr_tx_high: true,
      tx_attenuation_db: 0, tx_preemphasis_enabled: false, tx_save_wav: false, rx_deemphasis_enabled: false, rx_allow_legacy_grid: true, audio_backend: "alsa", collector_url: "",
      tx_quality: 10, tx_repair_pct: 5,
      tx_mode: "HIGH", tx_resize: "800x600",
      tx_free_w: 800, tx_free_h: 600,
      tx_speed: 6, tx_more_count: 5,
      tx_history_max: 100,
      overlays: makeDefaultOverlays(), active_overlay: 0,
    });
  }
  ensureOverlaySlots();
  ensureRadioSettings();
  const call = document.getElementById("callsign-input");
  if (call) call.value = currentSettings.callsign || "";
  // Fetch profile list from modem-core BEFORE any code that touches the
  // tx-mode / rx-forced-profile selects (they're empty in index.html).
  await loadModemProfiles();
  applyPttSettingsToUI();
  applyRxForceSettingsToUI();
  applyExperimentalModesToUI();
  const colUrl = document.getElementById("collector-url");
  if (colUrl) colUrl.value = currentSettings.collector_url || "";
  const histMax = document.getElementById("tx-history-max-input");
  if (histMax) histMax.value = String(currentSettings.tx_history_max ?? 100);
  const preemph = document.getElementById("tx-preemphasis-enabled");
  if (preemph) preemph.checked = !!currentSettings.tx_preemphasis_enabled;
  const saveWav = document.getElementById("tx-save-wav-enabled");
  if (saveWav) saveWav.checked = !!currentSettings.tx_save_wav;
  const deemph = document.getElementById("rx-deemphasis-enabled");
  if (deemph) deemph.checked = !!currentSettings.rx_deemphasis_enabled;
  const grid = document.getElementById("rx-allow-legacy-grid");
  if (grid) grid.checked = !!currentSettings.rx_allow_legacy_grid;
  const turbo = document.getElementById("rx-turbo-enabled");
  if (turbo) turbo.checked = !!currentSettings.rx_turbo;
  applyTurboModeStyling();
  const scrambler = document.getElementById("scrambler-enabled");
  if (scrambler) scrambler.checked = currentSettings.scrambler_enabled !== false;
  const alsaBackend = document.getElementById("audio-backend-alsa");
  if (alsaBackend) alsaBackend.checked = (currentSettings.audio_backend || "alsa") !== "cpal";
  const fdx = document.getElementById("full-duplex-enabled");
  if (fdx) fdx.checked = !!currentSettings.full_duplex_enabled;

  // SDR controls are built on demand by `renderSdrPanel` (called
  // from `loadDevices` once the dropdown selection is known). The
  // per-backend config is kept in `currentSettings.sdr_settings.backends[id]`
  // and surfaced through `data-sdr-field` inputs — no per-backend
  // load block here.
  applyTxSettingsToUI();
}

// Sync all persisted TX settings into txState and the UI. Called after
// loadSettings, so setupTxTab has already attached its listeners - we just
// update the values.
function applyTxSettingsToUI() {
  const intOr = (v, def) => Number.isFinite(v) ? v : def;
  const q = intOr(currentSettings.tx_quality, 10);
  const r = intOr(currentSettings.tx_repair_pct, 5);
  const sp = intOr(currentSettings.tx_speed, 6);
  const mc = intOr(currentSettings.tx_more_count, 5);
  const fw = intOr(currentSettings.tx_free_w, 800);
  const fh = intOr(currentSettings.tx_free_h, 600);
  const mode = currentSettings.tx_mode || "HIGH56";
  const resize = currentSettings.tx_resize || "800x600";

  txState.quality = q;
  txState.repairPct = r;
  txState.speed = sp;
  txState.moreCount = mc;
  txState.freeW = fw;
  txState.freeH = fh;
  txState.mode = mode;
  txState.resize = resize;

  const setVal = (id, v) => {
    const el = document.getElementById(id);
    if (el) el.value = String(v);
  };
  const setText = (id, v) => {
    const el = document.getElementById(id);
    if (el) el.textContent = String(v);
  };

  setVal("tx-quality", q);
  setText("tx-quality-val", q);
  setVal("tx-speed", sp);
  setText("tx-speed-val", sp);
  setVal("tx-repair-pct", r);
  setVal("tx-more-count", mc);
  setVal("tx-free-w", fw);
  setVal("tx-free-h", fh);

  const modeSel = document.getElementById("tx-mode");
  if (modeSel) modeSel.value = mode;

  for (const radio of document.querySelectorAll('input[name="tx-resize"]')) {
    radio.checked = radio.value === resize;
  }
  const freeWrap = document.getElementById("tx-resize-free");
  if (freeWrap) freeWrap.hidden = resize !== "free";

  refreshTxExperimentalWarn();
}

// ─── WAV-file replay (offline RX from a recorded capture) ─────────
//
// The Rust side exposes `start_capture_from_wav` which spins up a
// paced sender thread (500 ms batches at 48 kHz) feeding the same
// `Receiver<Vec<f32>>` the rx_worker reads from cpal/Pluto. The UI
// flow mirrors `startCapture` so the user gets the same
// state-chip / progress / level-meter behaviour with a WAV source.
function setupWavPlayback() {
  const btn = document.getElementById("btn-play-wav");
  const input = document.getElementById("rx-wav-input");
  if (!btn || !input) return;
  btn.addEventListener("click", () => {
    // Reset value so re-picking the same file still fires `change`.
    input.value = "";
    input.click();
  });
  input.addEventListener("change", () => {
    const file = input.files && input.files[0];
    input.value = "";
    if (file) startCaptureFromWav(file);
  });
  // Frontend echo of the pacer-finished event — flip the buttons back
  // so the user knows the file finished playing without having to
  // manually press Stop. The backend leaves the worker running for a
  // few seconds so any in-flight decode finalises naturally.
  if (window.__TAURI__ && window.__TAURI__.event) {
    listen("wav_playback_done", () => {
      logEvent("wav_playback_done", null);
    });
  }
}

function setupCaptureSubmitPanel() {
  const submit = document.getElementById("csp-submit");
  const dismiss = document.getElementById("csp-dismiss");
  if (submit) submit.addEventListener("click", submitPendingCapture);
  if (dismiss) dismiss.addEventListener("click", dismissCapturePrompt);
}

function wireEvents() {
  const names = [
    "preamble",
    "header",
    "app_header",
    "envelope",
    "progress",
    "file_complete",
    "session_end",
    "error",
    // Per-scan DSP breakdown: profile + ppm + per-segment sigma². One
    // entry per tick with decoded segments. Logged in the Info tab via
    // the generic logEvent path (JSON dump under the event name).
    "sf_detail",
  ];
  for (const name of names) {
    listen(name, (event) => {
      logEvent(name, event.payload);
      if (name === "file_complete") {
        showCurrentFile(event.payload);
        // Reveal-in-folder only for non-image files. For images we
        // already have the preview in the RX tab + the history, opening
        // the folder would be intrusive (focus stealing).
        if (!isImageMime(event.payload.mime_type)) {
          revealReceivedFile(event.payload.saved_path);
        }
      }
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
  listen("rx_realtime", (event) => {
    noteRxRealtime(event.payload);
  });

  // 0.10.43 : worker-requested capture restart. The worker emits this
  // whenever it transitions back to Idle (preamble-absence, EOT,
  // brickwall) -- in-process resets proved insufficient to re-arm RX
  // on a fresh signal. We mirror what a manual stop/start does : the
  // backend `restart_capture` command drops the cpal/SDR stream +
  // worker thread, and re-spawns with the same device + profile +
  // forced the operator originally picked.
  //
  // 0.10.45 : added auto_stop / auto_start `logEvent` markers around
  // the invoke so the operator can SEE in the GUI event log whether
  // the restart cycle actually ran (or where it got stuck). Manual
  // stop/start already logs "stop" + "start" entries from the
  // button handlers ; this gives the auto path the same visibility.
  listen("worker_requests_restart", async (event) => {
    const reason = (event.payload && event.payload.reason) || "?";
    logEvent("worker_requests_restart", { reason });
    logEvent("auto_stop", { reason });
    try {
      // 0.10.46 fix : `invoke` is not in scope inside wireEvents()
      // listener closures by default — must be destructured from
      // `window.__TAURI__.core` like every other usage in this file
      // (see e.g. main.js:133, 183, 711). Without this destructure
      // the listener threw `ReferenceError: Can't find variable:
      // invoke` and `worker_requests_restart_error` fired every
      // single Idle transition since 0.10.43, meaning the auto
      // stop/start NEVER actually ran.
      await invoke("restart_capture");
      logEvent("auto_start", { reason });
    } catch (err) {
      logEvent("worker_requests_restart_error", { reason, error: String(err) });
    }
  });

  // 0.10.38 : worker capture-state changes. Drives the status-bar chip
  // (`#v2-state-chip`) so the chip reflects the actual modem state
  // instead of staying permanently "idle". Also surfaced in the Info
  // event log so the operator sees the timestamped Idle ↔ Streaming
  // transitions alongside the rest of the per-tick telemetry.
  listen("modem_state", (event) => {
    const p = event.payload || {};
    updateV2State(p.active ? "streaming" : "idle");
    logEvent("modem_state", {
      active: p.active,
      profile: p.profile,
      t_ms: p.t_ms,
    });
  });

  // 0.10.38 : ±15 ppm safety-grid invocation. Emitted by the worker
  // every tick where Gardner + fast-path both failed and the grid
  // fired (lowpower fallback OR user-toggled high-power). Logs entry /
  // exit timestamps + duration + ppm estimate + quota counter so the
  // operator can audit grid usage from the GUI without tail-ing
  // /tmp/nbfm-worker.log.
  listen("grid_used", (event) => {
    const p = event.payload || {};
    const fmtTime = (ms) => {
      if (!ms) return "?";
      const d = new Date(ms);
      const hh = String(d.getHours()).padStart(2, "0");
      const mm = String(d.getMinutes()).padStart(2, "0");
      const ss = String(d.getSeconds()).padStart(2, "0");
      const fff = String(d.getMilliseconds()).padStart(3, "0");
      return `${hh}:${mm}:${ss}.${fff}`;
    };
    const ppm =
      p.drift_ppm === null || p.drift_ppm === undefined
        ? "?"
        : `${p.drift_ppm.toFixed(2)} ppm`;
    logEvent("grid_used", {
      in: fmtTime(p.t_start_ms),
      out: fmtTime(p.t_end_ms),
      duration_ms: p.duration_ms,
      n_passes: p.n_passes,
      ppm,
      fallback: p.fallback,
      quota: `${p.recent}/${p.quota}`,
    });
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
    // A different session_id means a genuinely new transmission is
    // starting — clear the previous burst's visuals so stale progress
    // bars / constellation / pilots don't linger over the new data.
    // A re-armed identical session_id (e.g. a worker restart on the same
    // burst) keeps the existing display so the operator doesn't lose
    // already-converged blocks visually.
    if (
      fountainState.sessionId != null &&
      fountainState.sessionId !== p.session_id
    ) {
      resetRxVisuals();
    }
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
    // Refresh the RX column of the History tab. Lightweight: one
    // read_dir + parsing each session's meta.json.
    refreshHistory().catch(() => {});
  });
  listen("tx_archived", () => {
    // Emitted by tx_worker::archive_payload at the start of every transmission.
    refreshHistory().catch(() => {});
  });

  // ── Radio tab telemetry (SDR sources only). The capture thread emits
  // these via the worker's radio session; we cache the latest frame and
  // the RAF loop (only running while the Radio tab is active) paints.
  listen("radio_spectrum", (event) => {
    radioState.rf = event.payload;
  });
  listen("radio_audio_spectrum", (event) => {
    radioState.audio = event.payload;
  });
  listen("radio_smeter", (event) => {
    const p = event.payload || {};
    // EMA-smooth so the needle doesn't jitter.
    const v = p.channel_power_dbfs;
    if (typeof v === "number" && isFinite(v)) {
      radioState.smeterDb =
        radioState.smeterDb === null ? v : radioState.smeterDb * 0.7 + v * 0.3;
    }
  });
  listen("radio_tune_state", (event) => {
    radioState.tune = event.payload;
    updateRadioTuneDisplay();
  });
  listen("radio_fm_excursion", (event) => {
    // Latest FM-excursion frame; the Radio-tab RAF loop folds it into the
    // scrolling over-modulation graph (drawFmExcursion).
    radioState.excursion = event.payload;
  });
  listen("radio_audio_level", (event) => {
    // Latest SSB level frame; folded into the scrolling level graph
    // (drawSsbLevel) when the chain is in SSB-USB mode.
    radioState.audioLevel = event.payload;
  });
}

// Kiosk mode (small touchscreen, e.g. Pi 7" 800x480) — the Rust setup
// hook auto-engages fullscreen and emits `kiosk_mode` so the frontend
// can switch its CSS layout and reveal the on-screen exit button.
function setupKioskMode() {
  // Add the body class based on viewport size — independent of any
  // Rust-side event. The Rust setup hook emits `kiosk_mode` *before*
  // the webview is loaded so the listener-driven path always loses
  // the race; we still register the listener for completeness (e.g.
  // when an event fires later from runtime), but the viewport check
  // is what actually drives the class on a fresh open.
  if (window.innerWidth <= 900 || window.innerHeight <= 600) {
    document.body.classList.add("kiosk-mode");
  }
  if (window.__TAURI__ && window.__TAURI__.event) {
    listen("kiosk_mode", () => {
      document.body.classList.add("kiosk-mode");
    });
  }
  const exitBtn = document.getElementById("kiosk-exit");
  if (exitBtn && window.__TAURI__ && window.__TAURI__.window) {
    exitBtn.addEventListener("click", async () => {
      try {
        await getCurrentWindow().close();
      } catch (e) {
        console.error("kiosk close", e);
      }
    });
  }
  // Escape toggles fullscreen on/off (kiosk mode only). The image
  // lightbox owns its own Escape handler and we yield to it when open.
  window.addEventListener("keydown", async (ev) => {
    if (ev.key !== "Escape") return;
    if (!document.body.classList.contains("kiosk-mode")) return;
    const lb = document.getElementById("image-lightbox");
    if (lb && !lb.hidden) return;
    if (!window.__TAURI__ || !window.__TAURI__.window) return;
    try {
      const win = getCurrentWindow();
      const isFs = await win.isFullscreen();
      await win.setFullscreen(!isFs);
    } catch (e) {
      console.error("kiosk toggle", e);
    }
  });
}

// Resolve the running app version from the Tauri backend (which reads it
// from tauri.conf.json — same string that ends up in the .deb) and paint
// it into the right-pinned chip in the tab bar. Fire-and-forget; on
// failure we fall back to the literal "?" so it's obvious the chip is
// live and just couldn't talk to the backend.
async function setupAppVersionChip() {
  const el = document.getElementById("app-version");
  if (!el) return;
  try {
    const v = await invoke("get_app_version");
    el.textContent = `v${v}`;
    el.title = `Version de l'application : ${v}`;
  } catch (e) {
    console.error("get_app_version", e);
    el.textContent = "v?";
  }
}

// Bridge the SDR subsystem's upward effects to the functions that still live
// here. lib/sdr.js emits on the bus (so it never imports a tab); the targets
// (persistSettings / loadDevices / the Radio renderers) are wired back here.
// Bus handlers for the RX/TX effects that still live in main.js (duplex bar,
// start button, realtime chip, raw-record state). The SDR/Settings halves
// (persist, reload devices, RX-warn) moved to tabs/settings.js and the Radio
// renderers to tabs/radio.js — they subscribe to the same events themselves.
function setupSdrBusHandlers() {
  onBus("tx:refresh-duplex-bar", () => { refreshDuplexTxBar(); });
  onBus("tx:recompress", () => { if (txState.sourceFile && !txState.fileMode) scheduleTxCompress(50); });
  onBus("tx:relay", (p) => { relayHistoryItem(p); });
  onBus("tx:resume", (p) => { resumeTxFromHistory(p); });
  onBus("tx:refresh-estimate", () => { refreshTxEstimate(); });
  onBus("rx:refresh-controls", () => { refreshRxDeviceLabel(); refreshStartButtonFromRx(); });
  onBus("capture:started", () => {
    refreshDuplexTxBar();
  });
  onBus("capture:stopped", () => {
    refreshStartButtonFromRx();
    refreshDuplexTxBar();
    noteRxRealtimeReset();
    refreshRawRecordingState();
  });
}

async function init() {
  // Load translations first so every subsequent setup* that reads
  // a `t(...)` (or HTML data-i18n) gets the right language out of
  // the gate — avoids a visible FR→EN flicker on EN-first launches.
  try { await initI18n(); } catch (err) { console.error("i18n", err); }
  setupSdrBusHandlers();
  setupLangSelect();
  setupKioskMode();
  setupSelectPicker();
  setupVirtKeyboard();
  setupTabs();
  setupAppVersionChip();
  setupLightbox();
  setupTxTab();
  setupSettingsTab();
  setupOverlaysTab();
  setupCaptureSubmitPanel();
  setupHistoryTab();
  await loadSettings();
  await loadSdrBackends();
  await renderSdrBackendsList();
  applyOverlaysToUI();
  setupChannelTab();
  setupSounderTab();
  await loadDevices();
  setupRadioTab();
  setupRadioSdrModal();
  refreshRadioTabVisibility();
  await loadSerialPorts();
  await loadSaveDir();
  // Display the initial PTT state (computed by the backend at setup).
  try {
    const st = await invoke("ptt_status");
    renderPttStatus(st);
  } catch (err) {
    console.error("ptt_status", err);
  }
  wireEvents();
  document.getElementById("btn-start").addEventListener("click", startCapture);
  document.getElementById("btn-stop").addEventListener("click", stopCapture);
  document.getElementById("btn-raw").addEventListener("click", toggleRawRecording);
  setupWavPlayback();
  window.addEventListener("resize", redrawAll);
  document
    .getElementById("btn-sessions-refresh")
    ?.addEventListener("click", refreshSessions);
  await refreshRawRecordingState();
  await refreshSessions();
  resetRxVisuals();
  // #HB9TOB: periodic tick to clear the OVD chip if no overdrive batch
  // has arrived for OVD_STICKY_MS (also useful when capture is stopped).
  setInterval(refreshOverdriveChip, 200);
  // Auto-start RX capture if a device is configured.
  await tryAutoStartCapture();
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
