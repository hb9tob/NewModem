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
import { ensureRadioSettings, radioState, refreshRadioTabVisibility, updateRadioTuneDisplay, tuneRadioTo, setupRadioTab, setupRadioSdrModal, startRadioRender, stopRadioRender, radioRfTicks, radioMarkerFrac, drawRadioRf } from "./tabs/radio.js";
import { refreshSettingsRxWarn, loadDevices, applyTurboModeStyling, applyRxForceSettingsToUI, loadModemProfiles, applyExperimentalModesToUI, applyPttSettingsToUI, loadSerialPorts, renderPttStatus, persistSettings, setupSettingsTab, loadSaveDir } from "./tabs/settings.js";
import { makeDefaultOverlays, ensureOverlaySlots, getActiveOverlayPayload, setupOverlaysTab, applyOverlaysToUI } from "./tabs/overlays.js";
import { setupHistoryTab, refreshHistory } from "./tabs/history.js";
import { OVD_STICKY_MS, lastProgress, lastPilotPhases, fountainState, showCurrentFile, revealReceivedFile, refreshRxDeviceLabel, refreshStartButtonFromRx, refreshRawRecordingState, toggleRawRecording, submitPendingCapture, dismissCapturePrompt, updateLevel, refreshOverdriveChip, noteAudioOverdrive, noteRxRealtime, noteRxRealtimeReset, noteProfileFromHeader, updateV2State, resetRxVisuals, hideFountainStatus, updateFountainStatus, updateV2Progress, redrawAll, drawProgressBlocks, drawPilotPhase, setProgressBitmap } from "./tabs/rx.js";

// Event log: we also keep an in-memory buffer so we can serialize and
// push it to the Phase D collector at submission time. Capped at 500
// entries like the DOM list.


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

// Settings tab: if RX is running, the worker has already opened the RX
// sound card and won't pick up a device change until the next start. We
// do NOT cut RX automatically (the user may just want to consult the
// tab) - we show a banner offering to stop RX and disable the RX device
// select to prevent a phantom change (other fields - pre-emphasis, TX
// device, callsign... - remain editable).

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

// ────────────────────────────────────────── Sessions tab (RaptorQ)
// Registry keyed by session_id (hex u32) — merged from :
//  - backend list_sessions command on load / refresh / tab click
//  - real-time session_armed / session_progress / session_decoded events






// ───────────────────────────────────────────────────── Received-file panel

// Open the file explorer on the received (selected) file. Uses
// tauri-plugin-opener, which handles Windows (explorer /select), Linux
// (D-Bus FileManager1, parent xdg-open fallback) and macOS (open -R).

// ─────────────────────────────────────────────────── Lightbox (double-click)
// Displays the image in OS fullscreen (Tauri setFullscreen) with wheel or
// keyboard zoom (up to 8x to inspect details) and drag/arrow pan.


// Tauri setFullscreen resolves before the WebView has propagated the new
// viewport. We wait for either a resize event or a safety timeout.



// Keeps the image at least partially inside the viewport:
//  - if it fits entirely (w <= vw / h <= vh), we center it;
//  - otherwise, we prevent it from sliding off-screen (at minimum one edge
//    touches an edge of the viewport).







// ─────────────────────────────────────────── Settings / device selection
// Both sound cards (RX/TX) + the callsign live in the Settings tab and are
// persisted via the Tauri get_settings / save_settings commands.

// Backfill the radio-settings block so the rest of the code can read
// currentSettings.radio.* unconditionally — covers the get_settings
// catch-fallback and any pre-`radio` settings file.

// Default empty overlay slots (mirrors `default_overlay_slots()` on the
// Rust side). Slot 0 is the immutable "Aucun" entry.


/// Populate a device dropdown with cpal soundcards followed by every
/// SDR backend's live devices. SDR entries are filtered by direction
/// — backends with `tx_supported=false` don't appear on the TX list.
/// Each `<option>` carries `data-backend="<id>"` (or `"audio"` for
/// cpal); `renderSdrPanel` reads that attribute to know which
/// capabilities to render.
///
/// `backendDevices` is a `Map<backend_id, DeviceDescriptor[]>`
/// produced by parallel `list_sdr_devices` calls in `loadDevices`.

// ─── SDR-agnostic panel rendering ────────────────────────────────
//
// Every per-backend control (frequency input min/max, AGC dropdown
// options, antenna selector, gain row layout, feature checkboxes,
// CTCSS tone picker, …) is built from `BackendCapabilities`
// returned by the Tauri command `list_sdr_backends`. The frontend
// has zero hardcoded knowledge of "Pluto" or "SDRplay" except for
// the small whitelist in `buildBackendExtrasRow`.

/// Cache of every compiled-in backend's static descriptor. Keyed by
/// `id`; values are `{id, display_name, capabilities}` shipped from
/// `sdr_registry::registered_backends`. Loaded once at startup.

/// EIA standard CTCSS tones (39 values, in Hz). Mirror of
/// `modem_sdr_dsp::ctcss_gen::EIA_CTCSS_TONES_HZ`. Used by every
/// backend whose `capabilities.features.ctcss_tx === true`.


/// Read the per-backend `enabled` flag from settings (defaults to
/// `false`). Single source of truth for "should the GUI bother
/// enumerating this backend".

/// Build the "Backends SDR" checkbox section in Paramètres. One row
/// per registered backend, status text fed by
/// `get_backend_library_status`. Toggling a checkbox persists the new
/// `enabled` flag and triggers an immediate `loadDevices()` so the
/// device dropdown reflects the change without a tab refresh.

/// Ping `get_backend_library_status` for one backend and update the
/// inline status span. Called on tab open + after every checkbox
/// toggle.

/// Per-device capability cache keyed by composite name (e.g.
/// "sdrplay:1500R76GR1"). Backends with multiple hardware variants
/// (SDRplay: RSPduo / RSP1A / RSP1B / RSP1) ship per-device caps via
/// the Tauri `get_sdr_device_capabilities` command — the antenna
/// selector / tuner radio / bias-T checkbox / LNA-state range follow
/// the actual hardware instead of the family lowest-common-denominator.
/// Backends without per-device variants (Pluto, RTL-SDR) return their
/// family caps unchanged — same JSON shape, same render path.

/// Resolve the right `capabilities` payload for a composite device
/// name, fetching + caching on first miss. `null` while in-flight; the
/// caller should re-render once `await`-resolved. Errors fall back to
/// the family caps so the panel still renders something useful.

/// Kick off a `resolveDeviceCaps` for the currently-selected SDR on
/// `<direction>-device-select`. Awaitable so the change listener can
/// re-render once the IPC reply comes back. No-op for cpal entries
/// or when nothing is selected.

/// Read the cached per-device caps for the currently-selected SDR on
/// `<direction>-device-select`, falling back to the backend's family
/// caps if nothing is cached yet (first render). Synchronous — the
/// `change` listener is the one that triggers the async refresh.

/// Read the `data-backend` attribute of the currently-selected
/// option on a device dropdown. Returns the backend ID for SDR
/// entries or `null` for cpal soundcard entries (which carry
/// `data-backend = "audio"`).

/// Ensure `currentSettings.sdr_settings.backends[backendId]` exists,
/// seeding from the per-backend defaults table (mirror of
/// `default_sdr_config_for` in `settings.rs`) on first selection.



/// Build the SDR panel rows for a direction ("rx" or "tx") from the
/// currently-selected device's backend capabilities. Hidden when
/// the selection is a cpal card or an RX-only backend on the TX
/// panel.






/// Pull the LNA state out of either gain shape — the manual
/// `lna_plus_if` payload, or the AGC variant's `lna_state` overlay.
/// Returns `null` when the current gain has no LNA dimension (e.g.
/// continuous-dB Pluto config).

/// True when the active AGC mode advertises `keeps_lna_manual`.
/// Used by `buildGainRow` to decide whether the LNA `<input>`
/// should stay enabled while AGC is on.










/// Generic change handler: read `data-sdr-field` + `-transform`,
/// write the parsed value back into the per-backend config and
/// persist immediately.





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

/// Toggle the very-dark-red background that signals the experimental turbo
/// reception path is selected. Driven purely by the `rx_turbo` setting (NOT
/// by the Power Mode checkbox, which is a separate, legacy-path knob), so the
/// operator always has an unmistakable visual cue of which RX decoder is live.


/// Profiles fetched from modem-core via the Tauri command list_modem_profiles.
/// Drives the contents of the tx-mode and rx-forced-profile combos so the GUI
/// never hard-codes the modem list — adding a profile in modem-core makes it
/// appear here with no JS/HTML change required.


/// (Re)builds the tx-mode and rx-forced-profile <select>s from the cached
/// profile descriptors. When the experimental toggle is OFF, profiles flagged
/// experimental are physically excluded — hiding via `hidden` is unreliable
/// across some Tauri WebViews. The previous select.value is preserved when
/// the matching option is still present.



/// Apply the state of the "Enable experimental modes" toggle:
/// - update the settings checkbox
/// - re-populate the profile combos with experimentals filtered in/out
/// - hide/show the "Force a profile" bar of the RX tab
/// If the user disables the toggle while rx_force_mode is ON, we disable
/// forced mode to avoid staying locked on an experimental profile with no
/// way to reach it. Same if the current persisted profile is experimental:
/// we fall back to HIGH56 (standard since 2026-04-28).

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






// Show/sync the Pi-only hardware playback-volume slider for the current
// TX device. Hidden unless the device exposes a controllable ALSA mixer
// (`tx_device_has_mixer`); when shown, seeds the slider from the card's
// live "Speaker" level (`get_tx_volume`). No-op on non-Tauri / non-Linux
// hosts (the commands return false/None and the row stays hidden).




// Start RX capture if it isn't already running, no TX is occupying the
// audio chain, and a valid RX device is selected. Called at app startup
// and when returning to the RX tab.

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







// ─────────────────────────────────────────── Submit capture (Phase D)
// If the user has set a collector URL in Settings, we show a panel right
// after the end of a raw capture to offer submission. Otherwise, nothing -
// we only submit on explicit request.




// ─────────────────────────────────────────── Overlays tab
// Single source of truth = `currentSettings.overlays` + `currentSettings.active_overlay`.
// Slot 0 is the immutable "Aucun" entry. Slots 1..=4 are user-editable
// templates. On every edit we update currentSettings, persist, refresh
// the slot label, and trigger a TX recompress so the preview matches
// what will be transmitted.













function setupCaptureSubmitPanel() {
  const submit = document.getElementById("csp-submit");
  const dismiss = document.getElementById("csp-dismiss");
  if (submit) submit.addEventListener("click", submitPendingCapture);
  if (dismiss) dismiss.addEventListener("click", dismissCapturePrompt);
}


// #HB9TOB: how long the OVD chip stays red after the last detection. The
// chip clears itself after this delay if no further batch is flagged
// overdrive. See OVERDRIVE_* on the Rust side for the detection threshold.




// ────────────────────────────────────────── RX realtime margin chip
//
// `rx_realtime` is emitted by `rx_worker` every ~2 s (see RxRealtimePayload
// in modem-worker/src/rx_worker.rs). We classify into three states so the
// chip in the top bar gives a colour-coded health pulse:
//
//   ok   : healthy — lag < 100 ms, last_batch < 500 ms, no fresh drops
//   warn : transient pressure — lag 100–300 ms OR last_batch 500–800 ms,
//          still no drops. The session_buffer absorbs the spike, samples
//          are intact.
//   err  : sustained overload — lag > 300 ms OR samples were dropped
//          since the previous tick. On a soundcard input this means the
//          30 s SPSC ring in cpal_capture overflowed (= the reader thread
//          was starved long enough that even 30 seconds of slack ran out)
//          AND/OR the worker hit the 5 min session_buffer brickwall.
//          Either way the worker has flushed and returned to idle —
//          the chip is informing the user that the previous capture
//          was lost. CPU can't keep up with the chosen profile.
//
// The chip auto-clears to `off` when capture stops (see noteRxRealtimeReset).
// Tooltip carries the raw numbers.







// ─────────────────────────────── Per-block progress + constellation state
// `sigma2` here means the per-window data-symbol σ² (frame-only,
// hard-decision residuals — what `result.sigma2_data` carries on the
// Rust side). The pilot-residual σ² stays internal to the demod (LLR
// scale) and is not surfaced to the operator. Field is kept named
// `sigma2` because every consumer of `lastProgress` already expects it.
// Parallel to lastPilotPhases : true for META segments (header replicated),
// false for DATA segments. drawPilotPhase paints META in a distinct colour
// so the operator can see the full frame layout at a glance.










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
}

// ────────────────────────────────────────────────────────────── TX tab (GUI)
// The backend wiring (AVIF encoding, TX launch, audio rendering) comes
// later. Here we only handle: file loading (picker + DnD), target
// dimensions with aspect ratio respected, state of the controls.
const txState = {
  sourceFile: null,
  sourceImage: null,
  sourceSize: 0,
  sourceUrl: null,
  mode: "HIGH",
  resize: "800x600",
  freeW: 640,
  freeH: 480,
  // Default 10: size/quality trade-off usable out-of-the-box on a NBFM pass.
  // Persisted across sessions (cf. applyTxSettingsToUI).
  quality: 10,
  // AVIF encoder speed, 1..=10. 6 = balanced (a few seconds on an SP7),
  // 1 = max compression/very slow, 10 = fast but larger file.
  speed: 6,
  // % of RaptorQ repair blocks added to the initial burst (0, 5, 10, 20,
  // 30, 50, 100). 5 = modest default (user bumps it as needed via
  // "TX more"). Persisted across sessions.
  repairPct: 5,
  // True when the source is already an AVIF: we emit the bytes as-is,
  // with no decoding or re-encoding (no loss, no CPU cycles).
  avifPassthrough: false,
  // True when the source is not an image - we switch to compress_file_zstd
  // (lossless) instead of compress_image. No image preview, no resizing,
  // 10 min limit instead of 5.
  fileMode: false,
  // Number of blocks to emit as a "More" burst (exact value, not a %).
  // User picks from a discrete select or enters a free value.
  // Typical use case: "I'm missing 5 blocks" -> count = 5.
  moreCount: 5,
  aspectLinked: true,
  txActive: false,
  // Additional fountain blocks to generate on TX more (% of code size).
  morePct: 20,
  // State of the in-progress TX session: kept between the initial TX and
  // successive "More" bursts so we can continue ESI without overlapping
  // packets already emitted. Reset when image or mode change.
  lastTx: null,  // { esiMax, mode }
  // Path of the tx_history archive backing the CURRENT session, learnt from
  // tx_start's return value (fresh TX) or tx_resume (resumed session). Used
  // to persist the ESI high-water (tx_set_next_esi) after every burst so the
  // fountain can be continued later. Null = not archived yet.
  archivePath: null,
  // When a session is resumed from history, the callsign that was used at
  // the original TX. It MUST be reused for the continuation bursts, else the
  // session_id (which depends on the callsign) wouldn't match and the RX
  // would treat the extra blocks as a brand-new session. Null = use the
  // current settings callsign (fresh sessions).
  resumeCallsign: null,
  compressedBytes: null,
  compressedUrl: null,
  compressing: false,
  compressTimer: null,
  compressSeq: 0,
  // True when a parameter (quality / resize / free dimensions) has been
  // modified since the last successful compression. Drives the "stale"
  // indicator + the warn style of the Recompute button.
  compressDirty: false,
  // Anti-reentrance guard: drop ignored while an image is loading
  // (avoids two parallel loadTxFileFromPath calls).
  loading: false,
  // Estimate computed by the backend after each compression or mode
  // change; drives TX button activation and the "estimated duration ·
  // block count" display.
  estimate: null,
  // Tracking of an in-progress transmission.
  progress: null,
  restartRxAfter: false,
};

// Invalidate the current TX session reference. Called whenever the source
// or mode changes (= a different session_id), so a stale archivePath /
// resumeCallsign / ESI high-water never leaks into the next transmission.
function clearTxSessionRef() {
  txState.lastTx = null;
  txState.archivePath = null;
  txState.resumeCallsign = null;
}

// Promise chain to serialize AVIF compressions. Without it, dropping an
// image while a compression is running launches a 2nd ravif speed-1
// encoder in parallel - enough to saturate RAM and freeze KDE on large
// images.
let _compressChain = Promise.resolve();

// Transport limits. Image: <= 100 kB + <= 5 min (warn > 2 min). Non-image
// file: <= 10 min (warn > 5 min), no extra size limit - duration is the
// real NBFM constraint.
const TX_HARD_BYTES = 100 * 1024;
const TX_HARD_SECONDS = 5 * 60;
const TX_WARN_SECONDS = 2 * 60;
const TX_FILE_HARD_SECONDS = 10 * 60;
const TX_FILE_WARN_SECONDS = 5 * 60;


function refreshTxExperimentalWarn() {
  const warn = document.getElementById("tx-experimental-warn");
  if (!warn) return;
  // Source of truth: ProfileDescriptor.experimental from modem-core (cf.
  // V3Modem in modem-core/src/v3_modem.rs). Adding/removing an experimental
  // profile in core automatically re-flags the warning here.
  const desc = modemProfiles.find((p) => p.name === txState.mode);
  warn.hidden = !(desc && desc.experimental);
}

function refreshTxButtons() {
  const btnTx = document.getElementById("tx-btn-tx");
  const btnStop = document.getElementById("tx-btn-stop");
  const btnMore = document.getElementById("tx-btn-more");
  const btnCompress = document.getElementById("tx-btn-compress");
  const repairPct = document.getElementById("tx-repair-pct");
  const moreCount = document.getElementById("tx-more-count");
  if (!btnTx) return;
  const hasSource = !!txState.sourceFile;
  const isFile = !!txState.fileMode;
  const hasCompressed = txState.compressedBytes != null;
  const est = txState.estimate;
  if (btnCompress) {
    btnCompress.disabled =
      !hasSource || txState.compressing || txState.txActive;
    if (txState.compressing) {
      btnCompress.textContent = isFile ? "Compression zstd…" : "Compression…";
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

  // Limites : image = 100 ko + 5 min ; fichier = 10 min (taille libre).
  const bytes = txState.compressedBytes || 0;
  const dur = est ? est.duration_s : 0;
  const hardSeconds = isFile ? TX_FILE_HARD_SECONDS : TX_HARD_SECONDS;
  const warnSeconds = isFile ? TX_FILE_WARN_SECONDS : TX_WARN_SECONDS;
  const tooBig = !isFile && bytes > TX_HARD_BYTES;
  const tooLong = dur > hardSeconds;
  const warn = dur > warnSeconds && !tooLong;

  const canTx = hasSource
    && hasCompressed
    && !txState.compressing
    && !txState.txActive
    && !tooBig
    && !tooLong;
  btnTx.disabled = !canTx;
  const hasPriorTx =
    txState.lastTx != null && txState.lastTx.mode === txState.mode;
  btnMore.disabled = !hasSource || txState.txActive || !hasPriorTx;
  btnMore.title = moreButtonTitle();
  btnStop.disabled = !txState.txActive;
  if (repairPct) repairPct.disabled = !hasSource || txState.txActive;
  if (moreCount) moreCount.disabled = !hasSource || txState.txActive;

  // TX button label + color depending on state.
  if (txState.txActive) {
    btnTx.textContent = t("tx.btn_running");
    btnTx.title = t("status.tx_in_progress");
  } else if (tooBig) {
    btnTx.textContent = t("tx.btn_too_big");
    btnTx.title = t("status.tx_oversize_kio", { size: (bytes / 1024).toFixed(1) });
  } else if (tooLong) {
    const limMin = isFile ? 10 : 5;
    btnTx.textContent = t("tx.btn_too_long", { limit: limMin });
    btnTx.title = t("status.tx_oversize_time", { dur: fmtSeconds(dur), limit: limMin });
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
  // Kiosk: WebKitGTK on the Pi 7" touchscreen turns the native
  // title-based tooltip into a sticky bubble that doesn't auto-hide
  // on tap-release. We suppress it here and show a JS-controlled
  // toast (showKioskInfoToast) on TX press instead. Desktop hover
  // tooltips are unaffected.
  if (document.body.classList.contains("kiosk-mode")) {
    btnTx.removeAttribute("title");
  }
}


// TX button tooltip: duration, N emitted, K required, K threshold.
// `dur` is `est.duration_s` (raw float seconds); we always format it
// through fmtSeconds so the bubble shows M:SS (rounded to whole
// seconds) instead of "18.453123…".
function txButtonTitle(est, dur, longTx) {
  if (!est) return "";
  const base = longTx ? t("tx.dur_long") : t("tx.dur");
  const k = est.k_source;
  const n = est.n_initial ?? est.total_blocks;
  const parts = [t("tx.dur_blocks", { base, dur: fmtSeconds(dur), n })];
  if (k != null && k !== n) {
    parts.push(t("tx.parts_with_k", { k }));
  }
  if (est.duration_s_k != null) {
    parts.push(t("tx.threshold_lossless", { dur: fmtSeconds(est.duration_s_k) }));
  }
  return parts.join(" · ");
}

// More button tooltip: additional blocks, expected duration.
function moreButtonTitle() {
  const est = txState.estimate;
  const count = computeMoreCount();
  if (!est || !est.seconds_per_cw) {
    return t("tx.emit_n_more", { count });
  }
  const dur = est.seconds_per_cw * count;
  return t("tx.emit_n_more_dur", { count, dur: fmtSeconds(dur) });
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
  const hasSource = !!txState.sourceFile;
  if (!hasSource) {
    if (info) info.textContent = "—";
    if (srcSize) srcSize.textContent = "—";
    if (cmpSize) cmpSize.textContent = "—";
    return;
  }
  if (txState.fileMode) {
    // No dimensions to display for a non-image file - show the original
    // filename and the current modem mode.
    if (info) {
      const cmpPart = txState.compressing ? " · zstd…" : "";
      info.textContent = `${txState.sourceFile.name} · zstd · ${txState.mode}${cmpPart}`;
    }
  } else if (txState.sourceImage) {
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
  }
  if (srcSize) srcSize.textContent = txFormatBytes(txState.sourceSize);
  if (cmpSize) {
    if (txState.compressing && txState.compressedBytes == null) {
      cmpSize.textContent = t("tx.compressing");
      cmpSize.classList.remove("tx-stale");
    } else if (txState.compressedBytes != null) {
      const ratio = txState.sourceSize > 0
        ? ` (${(txState.compressedBytes / txState.sourceSize * 100).toFixed(1)}%)`
        : "";
      const staleTag = txState.compressDirty ? t("tx.compress_stale_suffix") : "";
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
  // In file mode, keep the original name (including extension) - that's
  // what the RX expects to decompress and write the final file.
  if (txState.fileMode) {
    return (txState.sourceFile.name || "fichier.bin").slice(0, 60);
  }
  const base = txState.sourceFile.name.replace(/\.[^/.]+$/, "");
  // Envelope allows 64 UTF-8 bytes, leave a little margin.
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
  try {
    const est = await invoke("tx_estimate", {
      payloadBytes: txState.compressedBytes,
      mode: txState.mode,
      callsign: txState.resumeCallsign || currentSettings.callsign || "HB9XXX",
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
  // Bail before any state mutation if there's nothing to compress —
  // otherwise showTxBusyOverlay would leave a stuck overlay on an
  // empty preview when the impl exits early on `!sourceFile`.
  if (!txState.sourceFile) return Promise.resolve();
  // Show the overlay synchronously so the user gets immediate feedback,
  // even if `_runTxCompressImpl` only starts 1 microtask later through
  // the `_compressChain`. Without this, clicking "Recalculer" on a fast
  // image (or a small AVIF passthrough) would flash the spinner for a
  // single frame — same root cause as the file-pick path.
  showTxBusyOverlay();
  // Serialize via _compressChain: chain the new compression after the
  // current one, instead of letting ravif run twice in parallel (see
  // _compressChain above).
  const chained = _compressChain
    .then(() => _runTxCompressImpl())
    .catch((err) => logEvent("tx_compress_chain_error", { message: String(err) }));
  _compressChain = chained;
  return chained;
}

async function _runTxCompressImpl() {
  if (!txState.sourceFile) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const seq = ++txState.compressSeq;
  // A (re)compression prepares a new transmission: zero the RaptorQ block
  // counter so the previous TX's state (kept on screen since 0.15.4) doesn't
  // linger over the new one. Reset BOTH the block grid (lastProgress) AND the
  // fountain status bar (fountainState) — they show the count independently,
  // so clearing only the grid left the bar still reading the old total. A live
  // RX refills them on its next packet.
  setProgressBitmap({ bitmap: null, expected: 0, converged: 0, sigma2: null });
  const v2txt = document.getElementById("v2-progress-text");
  if (v2txt) v2txt.textContent = "—";
  hideFountainStatus();
  drawProgressBlocks();
  // Fresh compressed bytes ⇒ a different RaptorQ session_id (it hashes the
  // payload), so the previous session's ESI high-water + archive ref must NOT
  // carry over — otherwise the next TX continues the OLD fountain (esiMax+1)
  // instead of starting a brand-new session at ESI 0. Resume bypasses this
  // function entirely (it transmits the bit-exact archive without
  // recompressing), so every call here IS a new session.
  clearTxSessionRef();
  txState.compressing = true;
  const previewEl = document.getElementById("tx-preview");
  if (previewEl) previewEl.classList.add("compressing");
  refreshTxPreview();
  refreshTxButtons();
  // Force the browser to paint the loader before launching invoke().
  // Cap the wait with a timeout: on WebKitGTK (Linux) requestAnimationFrame
  // can stall right after a native drag-drop until the next user
  // interaction. Without the fallback, the compression hangs here forever —
  // `compressedBytes` is never set and the TX button stays disabled until
  // the operator clicks "Compresser" again (which generates the frame that
  // unblocks rAF). The timeout lets the compression proceed regardless;
  // worst case the spinner paints one frame late. Same defensive rationale
  // as the 1 s `load`-event fallback in the `finally` below.
  await new Promise((r) => {
    let done = false;
    const go = () => {
      if (done) return;
      done = true;
      r();
    };
    requestAnimationFrame(() => requestAnimationFrame(go));
    setTimeout(go, 100);
  });
  try {
    if (txState.fileMode) {
      logEvent("tx_compress_start", { mode: "zstd", source_len: txState.sourceSize });
      const result = await invoke("compress_file_zstd");
      if (seq !== txState.compressSeq) return; // stale
      txState.compressedBytes = result.byte_len;
      txState.compressedUrl = null;
      txState.compressDirty = false;
      logEvent("tx_compress_done", {
        mode: "zstd",
        source_len: result.source_len,
        byte_len: result.byte_len,
      });
    } else {
      // Defensive resync from the DOM (resize can diverge from txState).
      const checkedRadio = document.querySelector('input[name="tx-resize"]:checked');
      if (checkedRadio && checkedRadio.value !== txState.resize) {
        txState.resize = checkedRadio.value;
      }
      const dims = txTargetDims();
      if (!dims) return;
      logEvent("tx_compress_start", {
        mode: "avif",
        resize: txState.resize,
        target_w: dims.w,
        target_h: dims.h,
        quality: txState.quality,
        speed: txState.speed,
        passthrough: !!txState.avifPassthrough,
      });
      // An active overlay must be baked into the pixels, which requires
      // a real decode/re-encode. We'd love to do that on AVIF sources too,
      // but the `image` crate isn't built with the `avif` feature (no
      // libdav1d dependency), so decoding AVIF bytes Rust-side returns an
      // error — the compression then fails silently and `compressedBytes`
      // stays null, leaving the TX button disabled (regression after the
      // default-overlay seeding). Workaround: keep passthrough ON for AVIF
      // sources even when an overlay is active. The overlay is silently
      // dropped for AVIF inputs (no worse than pre-overlay behaviour);
      // non-AVIF sources still get the overlay baked in normally.
      const ov = getActiveOverlayPayload();
      const result = await invoke("compress_image", {
        opts: {
          target_w: dims.w,
          target_h: dims.h,
          quality: txState.quality,
          speed: txState.speed,
          passthrough: !!txState.avifPassthrough,
          overlay: txState.avifPassthrough ? null : ov,
        },
      });
      if (seq !== txState.compressSeq) return; // stale
      txState.compressedBytes = result.byte_len;
      const url = `${convertFileSrc(result.preview_path)}?v=${Date.now()}`;
      txState.compressedUrl = url;
      txState.compressDirty = false;
      const previewImg = document.getElementById("tx-preview-img");
      if (previewImg) {
        // Explicitly evict the previous decoded image from WebKit's image
        // cache before assigning the new src. Each Recalculer click bumps
        // `?v=Date.now()` so WebKit treats every URL as a brand-new
        // resource and would otherwise accumulate decoded AVIF buffers
        // in the WebProcess across encodes — observed 2026-05-29 :
        // 1-3 successful previews then a Wayland "Broken pipe" / WebKit
        // crash. `removeAttribute("src")` forces WebKit to drop the
        // previous decoded surface before the new fetch starts.
        previewImg.removeAttribute("src");
        previewImg.src = url;
      }
      logEvent("tx_compress_done", {
        mode: "avif",
        source_w: result.source_w,
        source_h: result.source_h,
        actual_w: result.actual_w,
        actual_h: result.actual_h,
        byte_len: result.byte_len,
      });
    }
    refreshTxEstimate();
  } catch (err) {
    if (seq === txState.compressSeq) {
      logEvent("tx_compress_error", { message: String(err) });
    }
  } finally {
    if (seq === txState.compressSeq) {
      txState.compressing = false;
      // Defer the lock release until the new preview image has finished
      // decoding in WebKit. Setting `previewImg.src = url` above only
      // *kicks off* an async fetch+decode in the WebProcess ; if we drop
      // `body.compressing-lock` right here, any clicks that piled up
      // during compression (when pointer-events were blocked from JS but
      // still queued at the WebKit event-pump level) get dispatched
      // simultaneously with libavif rendering — racing into a
      // WebProcess crash on WebKitGTK + libavif (observed 2026-05-29).
      // Waiting on the `load` event gives the decoder a clean window.
      // Falls back to a 1 s safety timer in case the image never fires
      // load/error (e.g. AVIF parse error swallowed silently).
      const release = () => {
        hideTxBusyOverlay();
        refreshTxPreview();
        refreshTxButtons();
      };
      const previewImg = document.getElementById("tx-preview-img");
      if (previewImg && previewImg.getAttribute("src") && !previewImg.complete) {
        let done = false;
        const wrapped = () => { if (!done) { done = true; release(); } };
        previewImg.addEventListener("load", wrapped, { once: true });
        previewImg.addEventListener("error", wrapped, { once: true });
        setTimeout(wrapped, 1000);
      } else {
        release();
      }
    }
  }
}

// Image-extension detection - if false, switch to the file/zstd flow.

// Render an 8-channel punched-tape visual from the filename (holes
// represent the actual ASCII bytes). Looping SMIL scroll for the retro
// vibe - pure decoration, no modem semantics. Placed in lieu of the
// image in file mode.
function renderFileTape(filename) {
  const container = document.getElementById("tx-file-tape");
  if (!container) return;
  const COL_W = 24;
  const ROW_Y = [16, 38, 60, 88, 110, 132, 154, 176];
  const SPROCKET_Y = 99;
  const TAPE_H = 192;
  // 30 bytes so the scroll loops even for short filenames. If the
  // filename is shorter, repeat it; if longer, truncate it.
  const N_BYTES = 30;
  const src = filename || "fichier.bin";
  const bytes = [];
  for (let i = 0; i < N_BYTES; i++) {
    bytes.push(src.charCodeAt(i % src.length) & 0xff);
  }
  const TAPE_W = N_BYTES * COL_W;
  const holes = [];
  for (let i = 0; i < N_BYTES; i++) {
    const b = bytes[i];
    const x = i * COL_W + COL_W / 2;
    holes.push(`<circle cx="${x}" cy="${SPROCKET_Y}" r="2.5" fill="#1a0e05"/>`);
    for (let bit = 0; bit < 8; bit++) {
      if ((b >> (7 - bit)) & 1) {
        holes.push(`<circle cx="${x}" cy="${ROW_Y[bit]}" r="5.5" fill="#1a0e05"/>`);
      }
    }
  }
  const holesSvg = holes.join("");
  container.innerHTML = `
    <svg viewBox="0 0 ${TAPE_W} ${TAPE_H}" preserveAspectRatio="xMidYMid meet" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <linearGradient id="tape-edge" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stop-color="#5a3a18" stop-opacity="0.55"/>
          <stop offset="0.04" stop-color="#5a3a18" stop-opacity="0"/>
          <stop offset="0.96" stop-color="#5a3a18" stop-opacity="0"/>
          <stop offset="1" stop-color="#5a3a18" stop-opacity="0.55"/>
        </linearGradient>
      </defs>
      <rect x="0" y="0" width="${TAPE_W}" height="${TAPE_H}" fill="#c9a574"/>
      <g>
        ${holesSvg}
        <g transform="translate(${TAPE_W} 0)">${holesSvg}</g>
        <animateTransform attributeName="transform" type="translate"
                          from="0 0" to="${-TAPE_W} 0" dur="9s"
                          repeatCount="indefinite"/>
      </g>
      <rect x="0" y="0" width="${TAPE_W}" height="${TAPE_H}" fill="url(#tape-edge)"/>
    </svg>
  `;
}

// Single state for the AVIF controls (resize / quality / speed): locked
// when the source is already an AVIF (passthrough) OR not an image
// (zstd). In both cases, these controls have no effect on emitted bytes.
function applyTxModeUI() {
  const passthrough = !!txState.avifPassthrough;
  const file = !!txState.fileMode;
  const lock = passthrough || file;
  const hint = document.getElementById("tx-passthrough-hint");
  if (hint) {
    if (file) {
      hint.hidden = false;
      hint.textContent = t("tx.file_zstd_hint");
    } else if (passthrough) {
      hint.hidden = false;
      hint.textContent = t("tx.passthrough_short");
    } else {
      hint.hidden = true;
    }
  }
  const previewImg = document.getElementById("tx-preview-img");
  if (previewImg) previewImg.style.display = file ? "none" : "";
  const tape = document.getElementById("tx-file-tape");
  if (tape) {
    tape.hidden = !file;
    if (file) {
      const name = (txState.sourceFile && txState.sourceFile.name) || "fichier.bin";
      renderFileTape(name);
    }
  }
  const ids = ["tx-quality", "tx-speed", "tx-free-w", "tx-free-h"];
  for (const id of ids) {
    const el = document.getElementById(id);
    if (el) el.disabled = lock;
  }
  for (const r of document.querySelectorAll('input[name="tx-resize"]')) {
    r.disabled = lock;
  }
}
// Aliases for compatibility with existing call sites.
function applyPassthroughUI() { applyTxModeUI(); }
function applyFileModeUI() { applyTxModeUI(); }

// Show the busy overlay immediately, before any async work. Pinning
// it on the preview area at the start of the load (rather than waiting
// for `_runTxCompressImpl` to add `.compressing` after the 50ms
// debounce + Promise chain) is what keeps the spinner from flashing
// for a single frame on fast images.
function showTxBusyOverlay() {
  const drop = document.getElementById("tx-drop-zone");
  const preview = document.getElementById("tx-preview");
  if (drop) drop.hidden = true;
  if (preview) {
    preview.hidden = false;
    preview.classList.add("compressing");
  }
  // Block ALL pointer events on the page during compression. Without
  // this, any click anywhere (a tab switch, a settings input, an
  // unrelated button) fired while WebKit is also handling the ravif/
  // libavif decoder for the new preview triggers a WebProcess crash
  // on this distro (observed 2026-05-29 : Wayland "Broken pipe" or
  // "Lost connection to compositor" after exactly one interaction
  // during the encode window). Keyboard still works (lets the user
  // cancel via Ctrl-W / Alt-F4 if they need). Cursor switches to
  // `progress` so it's obvious why clicks are ignored.
  document.body.classList.add("compressing-lock");
}
function hideTxBusyOverlay() {
  const preview = document.getElementById("tx-preview");
  if (preview) preview.classList.remove("compressing");
  document.body.classList.remove("compressing-lock");
}

// Load a file from a disk path (native Tauri drag-drop). The backend
// reads the bytes itself via set_tx_source_from_path: we completely
// avoid JSON-array IPC serialization which, on a large image, allocated
// ~10x the file size on both JS and Rust sides and could freeze KDE.
async function loadTxFileFromPath(path) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  // Anti-reentrance: ignore successive drops while a load OR compression
  // is in progress. Without the `compressing` check, dropping a new image
  // during a long ravif encode replaced the backend tx_source mid-flight
  // and piled `_runTxCompressImpl` calls on `_compressChain` until the
  // WebView ran out of memory.
  if (txState.loading || txState.compressing) {
    logEvent("tx_drop_ignored", { message: "loading or compression already in progress", path });
    return;
  }
  txState.loading = true;
  showTxBusyOverlay();
  const url = convertFileSrc(path);
  const name = path.split(/[/\\]/).pop() || "fichier";
  const isImage = isImageFilename(name);
  try {
    // Upload by path (no bytes through IPC).
    const size = await invoke("set_tx_source_from_path", { path });
    if (txState.sourceUrl) {
      URL.revokeObjectURL(txState.sourceUrl);
      txState.sourceUrl = null;
    }
    txState.sourceFile = { name, size };
    txState.sourceSize = size;
    txState.fileMode = !isImage;
    txState.avifPassthrough = isImage && name.toLowerCase().endsWith(".avif");
    txState.compressedBytes = null;
    txState.compressedUrl = null;
    txState.compressDirty = false;
    clearTxSessionRef();
    if (isImage) {
      // Load the image as preview via asset://.
      const img = new Image();
      await new Promise((resolve, reject) => {
        img.onload = () => resolve();
        img.onerror = () => reject(new Error(`image load failed: ${path}`));
        img.src = url;
      });
      txState.sourceImage = img;
      if (txState.resize !== "free") {
        txState.freeW = img.naturalWidth;
        txState.freeH = img.naturalHeight;
        const fw = document.getElementById("tx-free-w");
        const fh = document.getElementById("tx-free-h");
        if (fw) fw.value = txState.freeW;
        if (fh) fh.value = txState.freeH;
      }
    } else {
      txState.sourceImage = null;
    }
    applyPassthroughUI();
    applyFileModeUI();
    document.getElementById("tx-drop-zone").hidden = true;
    const preview = document.getElementById("tx-preview");
    const previewImg = document.getElementById("tx-preview-img");
    if (previewImg) previewImg.src = isImage ? url : "";
    if (preview) preview.hidden = false;
    refreshTxPreview();
    refreshTxButtons();
    scheduleTxCompress(50);
  } catch (err) {
    logEvent("tx_error", { message: `drop ${path}: ${err}` });
    // Compression won't run -- clear the overlay so it doesn't stay stuck.
    hideTxBusyOverlay();
  } finally {
    txState.loading = false;
  }
}

async function loadTxFile(file) {
  if (!file) return;
  // Anti-reentrance: same rationale as `loadTxFileFromPath` — without
  // this guard, picking a new image during a long ravif encode crashed
  // the WebView via piled-up `_compressChain` impls.
  if (txState.loading || txState.compressing) {
    logEvent("tx_pick_ignored", { message: "loading or compression already in progress" });
    return;
  }
  txState.loading = true;
  showTxBusyOverlay();
  // Release the previous blob URL if any.
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  const isImage = (file.type && file.type.startsWith("image/"))
    || isImageFilename(file.name || "");
  txState.sourceFile = file;
  txState.sourceSize = file.size;
  txState.fileMode = !isImage;
  txState.avifPassthrough = isImage && (
    file.type === "image/avif"
    || (file.name || "").toLowerCase().endsWith(".avif")
  );
  applyPassthroughUI();
  applyFileModeUI();
  txState.compressedBytes = null;
  txState.compressedUrl = null;
  txState.compressDirty = false;
  const url = URL.createObjectURL(file);
  txState.sourceUrl = url;
  const finishLoad = async () => {
    clearTxSessionRef();
    document.getElementById("tx-drop-zone").hidden = true;
    const preview = document.getElementById("tx-preview");
    const previewImg = document.getElementById("tx-preview-img");
    if (previewImg) previewImg.src = isImage ? url : "";
    if (preview) preview.hidden = false;
    refreshTxPreview();
    refreshTxButtons();
    // Upload source to the backend for later compressions.
    try {
      const buf = await file.arrayBuffer();
      await invoke("set_tx_source", { bytes: Array.from(new Uint8Array(buf)) });
      scheduleTxCompress(50);
    } catch (err) {
      logEvent("tx_error", { message: `upload source: ${err}` });
      hideTxBusyOverlay();
    } finally {
      txState.loading = false;
    }
  };
  if (isImage) {
    const img = new Image();
    img.onload = () => {
      txState.sourceImage = img;
      if (txState.resize !== "free") {
        txState.freeW = img.naturalWidth;
        txState.freeH = img.naturalHeight;
        const fw = document.getElementById("tx-free-w");
        const fh = document.getElementById("tx-free-h");
        if (fw) fw.value = txState.freeW;
        if (fh) fh.value = txState.freeH;
      }
      finishLoad();
    };
    img.onerror = () => {
      logEvent("tx_error", { message: `impossible de charger ${file.name}` });
      hideTxBusyOverlay();
      txState.loading = false;
    };
    img.src = url;
  } else {
    txState.sourceImage = null;
    finishLoad();
  }
}

async function resetTxFile() {
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  txState.sourceFile = null;
  txState.sourceImage = null;
  txState.sourceSize = 0;
  txState.avifPassthrough = false;
  txState.fileMode = false;
  applyPassthroughUI();
  applyFileModeUI();
  txState.compressedBytes = null;
  txState.compressedUrl = null;
  txState.compressDirty = false;
  clearTxSessionRef();
  txState.compressSeq++;
  if (txState.compressTimer) {
    clearTimeout(txState.compressTimer);
    txState.compressTimer = null;
  }
  // Clear busy state too: once reset, the user must be allowed to pick
  // a new file even if an abandoned ravif encode is still finishing on
  // the Rust side. Without this, the `loading || compressing` guard in
  // loadTxFile/loadTxFileFromPath would lock the picker until the
  // discarded compression returned.
  txState.loading = false;
  txState.compressing = false;
  const drop = document.getElementById("tx-drop-zone");
  const preview = document.getElementById("tx-preview");
  const previewImg = document.getElementById("tx-preview-img");
  const fileInput = document.getElementById("tx-file-input");
  if (preview) {
    preview.classList.remove("compressing");
    preview.hidden = true;
  }
  if (drop) drop.hidden = false;
  if (previewImg) previewImg.src = "";
  if (fileInput) fileInput.value = "";
  refreshTxPreview();
  refreshTxButtons();
  try {
    await invoke("clear_tx_source");
  } catch {
    // Doesn't matter: the JS state is already reset.
  }
}

function setupTxTab() {
  const drop = document.getElementById("tx-drop-zone");
  const fileInput = document.getElementById("tx-file-input");
  if (!drop || !fileInput) return;

  drop.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    const file = fileInput.files && fileInput.files[0];
    // Clear the value first so picking the same file twice still fires
    // a `change` event on the next pick (browsers dedupe on value).
    fileInput.value = "";
    if (file) loadTxFile(file);
  });

  // Drag-drop: on Linux/WebKitGTK the HTML5 dragover/drop events are not
  // reliably surfaced (the WM intercepts). We use the native Tauri v2
  // events (dragDropEnabled:true in tauri.conf.json), emitted at the
  // window level.
  if (window.__TAURI__ && window.__TAURI__.event) {
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
    // New mode -> new session (RaptorQ session_id depends on the mode).
    clearTxSessionRef();
    currentSettings.tx_mode = txState.mode;
    persistSettings();
    refreshTxPreview();
    refreshTxEstimate();
    refreshTxButtons();
    refreshTxExperimentalWarn();
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
      currentSettings.tx_resize = r.value;
      persistSettings();
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
  // change (blur/Enter): persist free dimensions without hammering the
  // disk during typing.
  const persistFree = () => {
    currentSettings.tx_free_w = txState.freeW;
    currentSettings.tx_free_h = txState.freeH;
    persistSettings();
  };
  freeW.addEventListener("change", persistFree);
  freeH.addEventListener("change", persistFree);

  const quality = document.getElementById("tx-quality");
  quality.addEventListener("input", () => {
    txState.quality = parseInt(quality.value, 10) || 0;
    document.getElementById("tx-quality-val").textContent = txState.quality;
    markCompressDirty();
  });
  // change = mouseup on the slider: natural moment to persist without
  // hammering the disk during the drag.
  quality.addEventListener("change", () => {
    currentSettings.tx_quality = txState.quality;
    persistSettings();
  });

  const speed = document.getElementById("tx-speed");
  const speedVal = document.getElementById("tx-speed-val");
  const speedHint = document.getElementById("tx-speed-hint");
  const speedLabel = (v) => {
    if (v <= 2) return t("tx.speed_very_slow");
    if (v <= 4) return t("tx.speed_slow");
    if (v <= 6) return t("tx.speed_balanced_2");
    if (v <= 8) return t("tx.speed_fast");
    return t("tx.speed_very_fast");
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
  speed.addEventListener("change", () => {
    currentSettings.tx_speed = txState.speed;
    persistSettings();
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
        txState.repairPct = 5;
      }
      currentSettings.tx_repair_pct = txState.repairPct;
      persistSettings();
      // Refresh estimate: duration and N depend on this %.
      refreshTxEstimate().catch(() => {});
      refreshTxButtons();
    });
  }

  const moreCountEl = document.getElementById("tx-more-count");
  if (moreCountEl) {
    moreCountEl.value = String(txState.moreCount || 5);
    const onMoreChange = () => {
      const v = parseInt(moreCountEl.value, 10);
      if (Number.isFinite(v) && v > 0) txState.moreCount = v;
      refreshTxButtons();
    };
    moreCountEl.addEventListener("input", onMoreChange);
    moreCountEl.addEventListener("change", () => {
      currentSettings.tx_more_count = txState.moreCount;
      persistSettings();
    });
    moreCountEl.addEventListener("change", onMoreChange);
  }
  refreshTxButtons();
}

// ────────────────────────────────────────────── TX orchestration (RX↔TX)
// Kiosk info toast — JS-controlled replacement for the native
// title-based tooltip on the TX button. WebKitGTK on the Pi 7"
// touchscreen keeps the native tooltip sticky on tap-release; we
// own the timing here and auto-hide after `durationMs`. No-op
// outside kiosk mode (desktop keeps the native hover tooltip).
let kioskInfoToastTimer = null;
function showKioskInfoToast(text, durationMs = 5000) {
  if (!document.body.classList.contains("kiosk-mode")) return;
  if (!text) return;
  let el = document.getElementById("kiosk-info-toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "kiosk-info-toast";
    el.className = "kiosk-info-toast";
    document.body.appendChild(el);
  }
  el.textContent = text;
  // Force reflow so the fade-in transition runs again on rapid
  // re-show (e.g. user taps TX twice in a row).
  void el.offsetWidth;
  el.classList.add("show");
  if (kioskInfoToastTimer) clearTimeout(kioskInfoToastTimer);
  kioskInfoToastTimer = setTimeout(() => {
    el.classList.remove("show");
    kioskInfoToastTimer = null;
  }, durationMs);
}

async function txStart() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  if (txState.txActive) return;
  if (!txState.estimate) {
    logEvent("tx_start_skipped", { reason: "pas d'estimation (compresse d'abord)" });
    return;
  }
  // Kiosk: show a transient info bubble (auto-hides after 5 s) with
  // the same content the desktop hover tooltip carries. The "long
  // transmission" wording uses the same threshold as refreshTxButtons
  // (file vs image mode have distinct warn/hard limits).
  {
    const est = txState.estimate;
    const dur = est.duration_s;
    const warnSeconds = txState.fileMode ? TX_FILE_WARN_SECONDS : TX_WARN_SECONDS;
    showKioskInfoToast(txButtonTitle(est, dur, dur > warnSeconds));
  }
  const rxStopBtn = document.getElementById("btn-stop");
  const rxWasActive = rxStopBtn && !rxStopBtn.disabled;
  // Half-duplex (default): stop RX before TX, then maybeRestartRx() picks
  // it up after tx_complete. Full-duplex (opt-in): leave RX running and
  // never set restartRxAfter so the post-TX path is a no-op.
  const fdx = !!currentSettings.full_duplex_enabled;
  if (rxWasActive && !fdx) {
    try {
      await invoke("stop_capture");
    } catch (err) {
      logEvent("tx_pre_stop_error", { message: String(err) });
    }
  }
  txState.restartRxAfter = rxWasActive && !fdx;
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
  // The ESI never rewinds. A plain "TX" emits a full initial burst worth of
  // FRESH blocks (n_initial = K + repair) starting at the session's ESI
  // high-water: 0 for a brand-new image (identical to the historical initial
  // burst), or the continuation point for a re-sent / resumed session. This
  // guarantees every TX (and every TX more) adds NEW fountain symbols rather
  // than re-emitting packets recipients already hold.
  const callsign = txState.resumeCallsign || currentSettings.callsign || "";
  const filename = getTxFilename();
  const nInitial = computeNInitial() || 1;
  const prior =
    txState.lastTx && txState.lastTx.mode === txState.mode
      ? txState.lastTx.esiMax + 1
      : 0;
  try {
    if (prior > 0) {
      // Continue the fountain through the existing (OTA-validated) tx_more
      // path — same session_id, fresh ESIs starting at `prior`. `nInitial`
      // is a whole PACKET_QUANTUM, so no extra rounding shifts the high-water.
      logEvent("tx_start_continue", { esi_start: prior, count: nInitial });
      await invoke("tx_more", {
        args: {
          mode: txState.mode,
          callsign,
          filename,
          tx_device: currentSettings.tx_device || "",
          esi_start: prior,
          count: nInitial,
        },
      });
      txState.lastTx = { mode: txState.mode, esiMax: prior + nInitial - 1 };
    } else {
      // Fresh session: tx_start archives the payload and returns the archive
      // path, against which we persist the ESI high-water for later resume.
      const archivePath = await invoke("tx_start", {
        args: {
          mode: txState.mode,
          callsign,
          filename,
          tx_device: currentSettings.tx_device || "",
          repair_pct: txState.repairPct,
        },
      });
      if (archivePath) txState.archivePath = archivePath;
      txState.lastTx = { mode: txState.mode, esiMax: nInitial - 1 };
    }
    await persistNextEsi();
  } catch (err) {
    logEvent("tx_start_error", { message: String(err) });
    txState.txActive = false;
    refreshTxButtons();
    await maybeRestartRx();
  }
}

// K RaptorQ = number of source codewords required for decoding.
// Provided directly by the backend via the estimate (k_source), or
// approximated through total_blocks for compatibility with an older
// backend.
function computeK() {
  const est = txState.estimate;
  if (!est) return null;
  if (est.k_source != null) return Math.max(4, est.k_source);
  if (est.total_blocks != null) return Math.max(4, est.total_blocks);
  return null;
}

// Full initial-burst block count = the number a plain "TX" emits. Prefer
// the backend's authoritative `n_initial` (already rounded up to a whole
// PACKET_QUANTUM via effective_packet_count); fall back to the K + repair
// approximation for an older backend that doesn't expose it.
function computeNInitial() {
  const est = txState.estimate;
  if (est && est.n_initial != null) return est.n_initial;
  const k = computeK();
  if (!k) return null;
  const pct = txState.repairPct || 0;
  return k + Math.floor((k * pct) / 100);
}

// Persist the current session's ESI high-water onto its tx_history archive
// so the fountain can be continued later (even after an app restart). No-op
// until the session has an archive path (set by tx_start / tx_resume).
async function persistNextEsi() {
  if (!txState.archivePath || !txState.lastTx) return;
  try {
    await invoke("tx_set_next_esi", {
      archivePath: txState.archivePath,
      nextEsi: txState.lastTx.esiMax + 1,
    });
  } catch (err) {
    logEvent("tx_next_esi_error", { message: String(err) });
  }
}

// Number of additional blocks to emit in a "More" burst. Read directly
// from the numeric input (presets via datalist, free input allowed).
function computeMoreCount() {
  const el = document.getElementById("tx-more-count");
  if (!el) return txState.moreCount || 5;
  const v = parseInt(el.value, 10);
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
  const rxStopBtn = document.getElementById("btn-stop");
  const rxWasActive = rxStopBtn && !rxStopBtn.disabled;
  // Same FDX gate as in txStart: keep RX running when full_duplex_enabled
  // is on, otherwise preserve the historical stop-then-restart dance.
  const fdx = !!currentSettings.full_duplex_enabled;
  if (rxWasActive && !fdx) {
    try {
      await invoke("stop_capture");
    } catch (err) {
      logEvent("tx_pre_stop_error", { message: String(err) });
    }
  }
  txState.restartRxAfter = rxWasActive && !fdx;
  txState.txActive = true;
  txState.progress = null;
  // Remember where we'll land after this burst (count packets starting at esiStart).
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
        // Reuse the original callsign on a resumed session, else the
        // session_id wouldn't match (see resumeCallsign).
        callsign: txState.resumeCallsign || currentSettings.callsign || "",
        filename: getTxFilename(),
        tx_device: currentSettings.tx_device || "",
        esi_start: esiStart,
        count: count,
      },
    });
    await persistNextEsi();
  } catch (err) {
    logEvent("tx_more_error", { message: String(err) });
    txState.txActive = false;
    refreshTxButtons();
    await maybeRestartRx();
  }
}

async function txStop() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    await invoke("tx_stop");
  } catch (err) {
    logEvent("tx_stop_error", { message: String(err) });
  }
}

async function maybeRestartRx() {
  if (!txState.restartRxAfter) return;
  txState.restartRxAfter = false;
  // Small delay to let the TX sound card release its handles before
  // opening the RX capture (especially when the same card is used).
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
      // K = blocks needed for decoding (RaptorQ source), N = emitted (K + repair).
      // Showing both helps the user understand why the duration goes beyond
      // the strict minimum and how much margin the repair provides.
      const k = est.k_source != null ? est.k_source : est.total_blocks;
      const n = est.n_initial != null ? est.n_initial : est.total_blocks;
      const dur = fmtSeconds(est.duration_s);
      const durK = est.duration_s_k != null ? t("tx.k_threshold_suffix", { dur: fmtSeconds(est.duration_s_k) }) : "";
      txt.textContent = t("tx.fountain_summary", { n, k, dur, durK });
    } else {
      txt.textContent = "—";
    }
    return;
  }
  const kTail = est && est.k_source != null ? ` · K=${est.k_source}` : "";
  txt.textContent = t("tx.progress_blocks", {
    sent: p.blocks_sent,
    total: p.total_blocks,
    tail: kTail,
    elapsed: fmtSeconds(p.elapsed_s),
    dur: fmtSeconds(p.duration_s),
  });
}

// ──────────────────────────── Full-duplex TX progress bar
//
// In full-duplex mode the bottom canvas keeps showing RX blocks and we
// push TX progress to a dedicated bar stacked just above it. Colour is
// linearly interpolated between violet (0 %) and logo blue (100 %) so
// the user can read the burst progress at a glance from any tab.

const TX_DUPLEX_VIOLET = { r: 0x5e, g: 0x35, b: 0xb1 }; // #5E35B1 start
const TX_DUPLEX_BLUE   = { r: 0x29, g: 0xb6, b: 0xf6 }; // #29B6F6 end (logo)

function lerpDuplexColor(frac) {
  const t = Math.max(0, Math.min(1, frac));
  const r = Math.round(TX_DUPLEX_VIOLET.r + (TX_DUPLEX_BLUE.r - TX_DUPLEX_VIOLET.r) * t);
  const g = Math.round(TX_DUPLEX_VIOLET.g + (TX_DUPLEX_BLUE.g - TX_DUPLEX_VIOLET.g) * t);
  const b = Math.round(TX_DUPLEX_VIOLET.b + (TX_DUPLEX_BLUE.b - TX_DUPLEX_VIOLET.b) * t);
  return `rgb(${r}, ${g}, ${b})`;
}


function isDuplexActive() {
  return !!currentSettings.full_duplex_enabled
    && rxIsRunning()
    && !!(txState && txState.txActive);
}

// Update / show / hide the dedicated TX bar based on the current state.
// Called from onTxProgress, onTxComplete, onTxError, and from every site
// that toggles RX running state (start_capture / stop_capture / WAV
// playback start) so the bar appears as soon as both are active and
// disappears immediately when one stops.
function refreshDuplexTxBar() {
  const bar = document.getElementById("tx-duplex-bar");
  const fill = document.getElementById("tx-duplex-fill");
  if (!bar || !fill) return;
  if (!isDuplexActive() || !txState.progress) {
    bar.hidden = true;
    return;
  }
  const p = txState.progress;
  const total = p.total_blocks || 0;
  const sent = p.blocks_sent || 0;
  const frac = total > 0 ? Math.min(1, sent / total) : 0;
  fill.style.width = `${(frac * 100).toFixed(2)}%`;
  fill.style.backgroundColor = lerpDuplexColor(frac);
  bar.hidden = false;
}

function onTxProgress(payload) {
  txState.progress = payload;
  updateTxProgressText();
  if (isDuplexActive()) {
    // FDX: leave the bottom canvas to RX, paint TX in the dedicated bar.
    refreshDuplexTxBar();
    return;
  }
  // Half-duplex (or RX not running): reuse the bottom progress bar in TX
  // mode, same behaviour as before.
  const bitmap = new Uint8Array(Math.ceil((payload.total_blocks || 0) / 8));
  for (let i = 0; i < payload.blocks_sent; i++) {
    bitmap[i >> 3] |= 1 << (i & 7);
  }
  setProgressBitmap({
    bitmap,
    expected: payload.total_blocks,
    converged: payload.blocks_sent,
    sigma2: null,
  });
  drawProgressBlocks();
}

async function onTxComplete(payload) {
  logEvent("tx_complete", payload);
  txState.txActive = false;
  txState.progress = null;
  updateTxProgressText();
  refreshTxButtons();
  refreshDuplexTxBar();
  try {
    await invoke("tx_reset");
  } catch (_) {}
  // Keep the final RX raptor grid + constellation on screen after the
  // transmission ends (loopback / full-duplex): the operator wants to see
  // the last decoded state. The visuals are cleared only when a genuinely
  // new session arrives (session_armed with a different session_id).
  await maybeRestartRx();
}

async function onTxError(payload) {
  logEvent("tx_error", payload);
  txState.txActive = false;
  txState.progress = null;
  updateTxProgressText();
  refreshTxButtons();
  refreshDuplexTxBar();
  try {
    await invoke("tx_reset");
  } catch (_) {}
  // Preserve the final RX raptor grid + constellation (see onTxComplete).
  await maybeRestartRx();
}

// ─────────────────────────────────────────── Channel tab (cascade ATT)
// Phase A: a single persistent setting (tx_attenuation_db in Settings),
// fed either manually via the slider or by the median of a list of
// feedbacks received during QSO. Cascade list: JS session only.









// ─────────────────────────────────────────── History tab
// Unified TX (files emitted, archived at each tx_start) and RX (decoded
// sessions) view. "↻ Relay" button on each thumbnail for the emergency-
// radio mode: reload a file in the TX tab and propagate it further on
// the network.




async function relayHistoryItem(absolutePath) {
  // Bascule sur l'onglet TX puis recharge le fichier comme un drag-drop.
  const txBtn = document.querySelector('.tab-bar .tab[data-tab="tx"]');
  if (txBtn) txBtn.click();
  try {
    await loadTxFileFromPath(absolutePath);
  } catch (err) {
    logEvent("history_relay_error", { path: absolutePath, message: String(err) });
  }
}

// Resume a past TX session (TX history card "Compléter" button): reload the
// bit-exact archived payload WITHOUT recompressing, restore mode / callsign /
// filename / ESI high-water, and arm both TX and TX more on the SAME session.
// Clicking TX then emits a full fresh burst from where we left off; partial
// or late recipients top up their fountain. The session_id is reproduced
// automatically by the deterministic envelope (same payload + filename +
// callsign + mode).
async function resumeTxFromHistory(archivePath) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const txBtn = document.querySelector('.tab-bar .tab[data-tab="tx"]');
  if (txBtn) txBtn.click();
  let info;
  try {
    info = await invoke("tx_resume", { archivePath });
  } catch (err) {
    logEvent("tx_resume_error", { path: archivePath, message: String(err) });
    alert(`Reprise impossible : ${err}`);
    return;
  }
  if (txState.sourceUrl) {
    URL.revokeObjectURL(txState.sourceUrl);
    txState.sourceUrl = null;
  }
  // Restore the session into txState. We do NOT recompress: the archive IS
  // the wire payload, and the backend tx_payload_path now points at it.
  txState.mode = info.mode;
  const modeSel = document.getElementById("tx-mode");
  if (modeSel) modeSel.value = info.mode;
  txState.sourceFile = { name: info.filename, size: info.byte_len };
  txState.sourceSize = info.byte_len;
  txState.sourceImage = null;
  txState.fileMode = !info.is_image;
  txState.avifPassthrough = info.is_image; // archived images are AVIF
  txState.compressedBytes = info.byte_len;
  txState.compressedUrl = null;
  txState.compressDirty = false;
  txState.repairPct = Number.isFinite(info.repair_pct) ? info.repair_pct : txState.repairPct;
  // Continuation state: reuse the archived callsign + ESI high-water.
  txState.archivePath = info.archive_path || archivePath;
  txState.resumeCallsign = info.callsign || null;
  txState.lastTx = info.next_esi > 0
    ? { mode: info.mode, esiMax: info.next_esi - 1 }
    : null;
  // UI restore.
  applyPassthroughUI();
  applyFileModeUI();
  refreshTxExperimentalWarn();
  const dropZone = document.getElementById("tx-drop-zone");
  if (dropZone) dropZone.hidden = true;
  const preview = document.getElementById("tx-preview");
  const previewImg = document.getElementById("tx-preview-img");
  if (previewImg) {
    previewImg.removeAttribute("src");
    previewImg.src = info.is_image ? `${convertFileSrc(archivePath)}?v=${Date.now()}` : "";
  }
  if (preview) preview.hidden = false;
  const repairEl = document.getElementById("tx-repair-pct");
  if (repairEl) repairEl.value = String(txState.repairPct);
  refreshTxPreview();
  // The estimate (re)enables the TX button and gives n_initial for the
  // continuation burst; it reads txState.compressedBytes set above.
  await refreshTxEstimate();
  refreshTxButtons();
  logEvent("tx_resumed", {
    session_id: info.session_id,
    next_esi: info.next_esi,
    mode: info.mode,
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

// ───────────────────────────────────────────────────────── Radio tab
//
// SDR-only receiver cockpit: needle S-meter, frequency control with
// hybrid digital/LO tuning (handled backend-side), audio spectrum +
// soundcard monitoring, wideband RF spectrum + waterfall. The backend
// streams telemetry events (radio_spectrum / radio_audio_spectrum /
// radio_smeter / radio_tune_state) which `wireEvents` caches into
// `radioState`; a RAF loop (only alive while the tab is visible) paints.


// Waterfall colour LUT: black → blue → cyan → green → yellow → red →
// white. 256 entries of [r,g,b].


// ── S-meter calibration (approximate). The backend reports the channel
// power in dBFS; the S scale needs an absolute level in dBm at the SMA
// input. The Pluto/AD9363 front-end is roughly linear in its manual
// "hardwaregain", so:
//
//   P_dBm ≈ channel_power_dbfs − gain_dB + PLUTO_FS_DBM + trim_dB
//
// PLUTO_FS_DBM is the input power that drives the ADC to 0 dBFS at 0 dB
// gain — taken as ~0 dBm, a coarse board constant. The operator nudges
// `trim_dB` (the "Cal S" slider) to align the scale on a signal of known
// level, which absorbs the per-unit offset we can't know a priori.

// IARU Region 1 S-meter standard above 30 MHz: S9 = −93 dBm, one S-unit
// = 6 dB. All band presets here are VHF/UHF, so this is the reference.

// Best-effort effective RX gain in dB, derived from the active backend's
// current gain setting — feeds the S-meter dBFS→dBm conversion. Exactness
// isn't critical (the "Cal S" trim absorbs the offset); we just want the
// needle to track gain changes in roughly the right direction.


// dBFS channel power → estimated antenna power in dBm.

// dBm → continuous S-unit value (S9 = 9, S9+20 dB = 12.33, …).








// Reflect the persisted Radio-tab monitoring settings into the controls
// (values + labels). Does not call the backend — pushRadioControlsLive()
// handles that once a session is up. Called at setup and on tab entry.

// Push the persisted monitoring settings to the live SDR session. Safe to
// call with no active capture — invokeRadio soft-ignores "Radio
// indisponible". Run on Radio-tab entry so a freshly-started session
// picks up the squelch / monitor device / volume the operator last used.

// Build the backend-aware gain / AGC controls into the Radio-tab control
// bar, for the active RX backend. Reuses the same row builders as the
// Settings panel (continuous dB / LNA+IF / discrete ladder + real AGC
// modes). Persists to cfg.gain; the host's delegated `change` listener
// sends it live.

// Push the active backend's current gain setting to the running capture
// (live LNA/IF/AGC change). No-op when no SDR session is active.

// ── Radio-tab SDR-parameters popover ("⚙ Réglages SDR" under the S-meter).
// Reuses the same per-backend row builders as the Settings tab, rendered for
// the active RX backend. Live-tunable params (gain / squelch / width) are
// already on the bottom control bar; this popover covers the rest (AGC mode,
// antenna, LNA/IF, bias-T / notches, backend extras), most of which apply on
// the next RX start — hence the note + restart button.



// Stop then restart the RX capture so persisted SDR-config changes (antenna,
// AGC, bias-T, decimation, …) take effect. Frequency / gain are live and
// don't need this.



// Pre-fill the Radio-tab frequency entry from the active RX backend's
// persisted rx_freq_hz when no live session is driving the display yet.
// This closes the round-trip with tuneRadioTo (which persists the dialed
// RF): reopening the app shows the last listening frequency. A running
// session takes over via radioState.tune, so only seed when absent and the
// input is still blank — never clobber what the operator is typing.



// The dial spans S1 (left) … S9+60 dB (right). Ticks are placed at their
// true position on the dB scale (S-units 1..9 are 6 dB apart, the +20/+40/
// +60 over-S9 marks are 20 dB apart), so the needle reads a calibrated S.



// Human-readable S report: "S7", "S9", "S9+18 dB"…

// Generic line/area spectrum painter. `frame.bins_db` are dBFS bins.

// Pick a "nice" tick step (1/2/5 × 10ⁿ) so the frequency ruler lands on
// round MHz/kHz values rather than arbitrary fractions.

// Tick frequencies (Hz) spanning an RF frame's displayed band.

// Faint vertical frequency gridlines over a fully-repainted canvas (RF
// line spectrum). Not used on the waterfall, whose scroll buffer would
// smear them.
function drawRfGridlines(canvasId, frame) {
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

// The shared Hz ruler painted between the RF spectrum and the waterfall.

// Horizontal fraction [0..1] of the tuned frequency within the displayed
// RF band. The band is centred on the LO (frame.center_hz = lo); the tuned
// frequency is displayed_rf = lo + digital_offset, so the marker slides off
// centre as the NCO moves while the LO (and the band) stay put.

// Thin vertical line at the tuned frequency, drawn over a fully-repainted
// canvas (the RF spectrum). The waterfall gets its own scrolling marker
// column inside drawRadioRf.
function drawTunedMarker(canvasId) {
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

// Map a horizontal pixel on an RF canvas to an absolute frequency (Hz),
// using the latest RF frame's center/span. Returns null with no frame.

// Snap a click to the real signal: take the clicked frequency, find the
// strongest RF bin within ±SNAP_WINDOW_HZ, and use it only if that bin
// stands clearly above the window's mean level (a genuine peak). On a peak
// we refine the frequency to sub-bin accuracy by parabolic interpolation
// (FFT bins are ~280 Hz wide) and round to 100 Hz — so a carrier at
// 145.312550 stays on 145.3125, not collapsed to 145.312. If no peak is
// near the click — empty/flat spectrum — fall back to the clicked
// frequency rounded to the nearest kHz.


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

// ─── Touch-friendly <select> picker (kiosk) ───────────────────────
//
// WebKitGTK on Wayland renders `<select>` as a native popup whose
// height is capped to ~5-6 rows on the 800x480 Pi DSI panel. With
// 10 entries in `tx-mode` / `rx-forced-profile` the bottom of the
// list (where the experimental profiles live) is unreachable on a
// touchscreen — the popup is scrollable in theory but the affordance
// is invisible, so the user thinks the experimentals are gone.
//
// Fix: in kiosk mode we capture `mousedown` on every `<select>`
// before the engine opens its popup, and present a fullscreen modal
// instead — same options, ≥48 px tap targets, scroll obvious. Off
// kiosk this stays dormant, so the desktop UX is untouched.
//
// Opt-out: `data-select-picker-skip="1"` on the `<select>`.






// ─── Native virtual keyboard (kiosk text/number entry) ────────────
//
// In kiosk mode (no physical keyboard on the 7" Pi panel) text/number
// inputs become impossible to fill: the user can't enter their callsign,
// a filename, a Pluto frequency offset, etc. This component is a pure
// in-app touch keyboard — no system dependency (no squeekboard / wvkbd /
// onboard) — that auto-opens when an `<input>` is focused while
// `body.kiosk-mode` is set. Outside kiosk it stays dormant: a physical
// keyboard typing into the input works like before.
//
// Layouts:
//   * `alpha`   QWERTY uppercase + lower toggle, with shift, space,
//               and a `?123` key to switch to symbols/numerics.
//   * `symbols` digits, common punctuation, `ABC` to come back.
//   * `numeric` 0-9 + decimal point + sign for `<input type="number">`.
//
// Special-cased by input id:
//   * `callsign-input` opens caps-locked, ASCII letters + digits only.
//
// On Valider: write `virtKb.draft` to `target.value`, dispatch `input`
// + `change`, close. On Annuler / outside-tap / Esc: close, no save.













/// Pick the per-backend MRU list. Reads the `data-backend`
/// attribute on the input — set by the row builders to the backend
/// ID. Returns the live array (mutable) so callers can splice.





/// Push a freshly-validated frequency (MHz) onto the per-backend
/// MRU list. Dedup on Hz equality, prepend, cap at 6. Persists via
/// `save_settings` so the next keypad open already shows it. The
/// MRU bucket is the live array under
/// `currentSettings.sdr_settings.backends[backendId].freq_favorites`.

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}

// ────────────────────────────────────────────────── Sounder tab
// Ported from feat/modem-2x (channel-sounder feature, 2026-05-21).
// Backend lives in modem-worker-base/src/sounder.rs + the Tauri
// commands at the bottom of src-tauri/src/main.rs.

// Two Tauri commands back this UI:
//   - sounding_tx_render(request) → builds probe.wav + schedule.json
//     under <save_dir>/sounder/<id>/. The user plays the WAV via the RX
//     tab's "▶ Lire WAV" feature (loopback test) or an external player.
//   - sounding_analyze(capture_wav, schedule_json, family, metadata,
//     sync_threshold) → cross-correlates the chirp anchor, runs each
//     probe's analyser, writes <capture>.signature.json next to the WAV
//     and returns the ChannelSignature payload we render below.
//
// File-IO uses plain string paths (no plugin-dialog in this build).
// Defaults: a sane probe sequence covering SNR, IMD3, group delay,
// noise floor, frequency response, and a 10-step level sweep so the
// operator gets P1dB + sweet-spot estimates from a single TX render.

// The reference TX level grid: 11 amplitudes from -30 to 0 dBFS in 3 dB
// steps. The orchestrator emits every probe family at every level so
// the analyser can compare each metric (SNR, IMD3, BW, group delay,
// impulse response) against TX level — and flag the operator when
// the TX rig is set too loud (the typical mistake: peak well above
// the sweet spot, which produces gross IMD3 + clipping artefacts).

// Convert a dBFS level to a linear amplitude scale factor.

// Build the multi-level expansion of a probe family. `factory(amp)`
// returns a ProbeSpec for one level given the linear amplitude scale.
// Returns `{ probes, levels }` parallel arrays so the caller can pass
// the per-probe level_db across to the Rust side.

// Returns `{ probes, levels }`. `levels[i]` is the intended TX dBFS
// for `probes[i]`, or `null` for multi-level probes (level_sweep)
// where the level is encoded internally.



// Build the standard SoundingRequest the TX emit + RX regenerate
// paths share. Deterministic at the JS level: the same defaults yield
// byte-identical schedule.json + probe audio on TX and RX machines.

// TX-side: one-shot button that builds the standard probe sequence
// and plays it directly through the TX soundcard configured in the
// Paramètres tab (single source of truth — same field used by
// `txStart` for the regular modem TX). No files written, no fields
// to fill — the user just clicks once and waits.

// RX-side helper: regenerate the reference probe.wav + schedule.json
// locally with the standard parameters, then auto-fill the analyse
// pane's schedule path. The generator is deterministic, so the bytes
// match whatever the TX side emitted with the same JS — no file
// transfer between machines required.

// Stitch the RX chain-metadata form fields into a free-text equipment
// + notes string so the analyser side can persist them in the
// signature JSON. The Rust `SoundingMetadata` keeps a flat shape
// (text fields only) for backwards compatibility with the legacy
// schedule.


// Holds the most recent successful sounder analysis so the "Envoyer au
// collector" button can ship it without re-running anything. Reset
// implicitly each time `runSounderAnalyze` succeeds; we never clear it
// on error so a transient analyse-fail doesn't wipe a good result.

// Phone-by-phone manual: the RX operator reads `#sd-reco-att` to the
// TX operator, who types it into Channel → Cascade. This button does
// NOT touch local tx_attenuation_db (the local machine is the receiver,
// it isn't the one transmitting); it only ships the result to the
// shared collector at hb9tob.duckdns.org.

// Integrated capture + analyse toggle for the RX panel. Independent
// of the modem rx_worker — the Sounder tab stops the worker so the
// audio device is free. First click = spin up a standalone cpal/SDR
// → WAV writer; second click = stop the writer, finalise the WAV,
// then immediately fire the analyser. The WAV path auto-fills the
// analyse input.
// True when the current sounder capture is tapping the live SDR session via
// the raw-capture tee (start_raw_recording) rather than opening the device
// itself (sounding_rx_start_capture). Decides which stop command to call.

// ─────────────── Sounder plots (inline SVG, no external lib)
//
// Renders 6 channel-characterisation plots into <svg> tags inside the
// results panel after a successful sounding_analyze run. Each plot is
// self-contained: SVG nodes are wiped + rebuilt every call (cheap,
// the panels are small).

// Generic XY plotter. `points` = [[x, y], …]. Auto-scales axes unless
// `xMin/xMax/yMin/yMax` are supplied. Optional secondary trace, sweet-
// spot vertical line, and y=x reference diagonal.

// Render the 6 sounder plots from a signature object. Walks the
// measurements array to find the right per-family instance for each
// plot (sweet-spot for chirp/multitone; highest-peak for Golay).

