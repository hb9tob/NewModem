// Shared lifecycle — the audio-capture start/stop the RX/TX/Radio/Channel tabs
// all sequence on the one audio device. Pure lifecycle: it invokes the Rust
// start/stop, sets the start/stop button state, and EMITS capture:started /
// capture:stopped on the bus so the tabs refresh their own chips/bars (it never
// imports a tab). tx-coupled maybeRestartRx and channel-coupled
// stopRxAndTxForChannelTab stay with their tabs and call these.
import { invoke } from "./ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "./state.js";
import { logEvent } from "./log.js";
import { emit } from "./bus.js";

export async function startCapture() {
  const select = document.getElementById("rx-device-select");
  const deviceName = select ? select.value : "";
  const status = document.getElementById("status");
  if (!deviceName) {
    status.textContent = t("status.select_rx_in_settings");
    status.style.color = "#ef5350";
    return;
  }
  const forced = !!currentSettings.rx_force_mode;
  // If forced, pass the chosen profile; otherwise HIGH (default anchor,
  // auto-detection will refine it).
  const profile = forced ? (currentSettings.rx_forced_profile || "HIGH") : "HIGH";
  try {
    await invoke("start_capture", { deviceName, profile, forced });
    status.textContent = forced
      ? t("status.capture_forced", { profile })
      : t("status.capture_running");
    status.style.color = "#ffb74d";
    document.getElementById("btn-start").disabled = true;
    document.getElementById("btn-stop").disabled = false;
    if (select) select.disabled = true;
    // RX came up: tabs refresh their warn-bar / duplex bar / live radio
    // controls (the latter soft-ignored for non-SDR sources).
    emit("capture:started");
    logEvent("start", { device: deviceName, profile, forced });
  } catch (err) {
    status.textContent = t("status.error_start", { err });
    status.style.color = "#ef5350";
    logEvent("error", { message: String(err) });
  }
}

export async function tryAutoStartCapture() {
  const stopBtn = document.getElementById("btn-stop");
  const startBtn = document.getElementById("btn-start");
  const txStopBtn = document.getElementById("tx-btn-stop");
  if (!stopBtn || !startBtn) return;
  if (!stopBtn.disabled) return;
  if (txStopBtn && !txStopBtn.disabled) return;
  if (startBtn.disabled) return;
  await startCapture();
}

export async function startCaptureFromWav(file) {
  const status = document.getElementById("status");
  // Refuse to start a WAV replay when a live RX is already in flight —
  // the backend rejects it anyway, but this surfaces a clearer message
  // and avoids spending seconds reading the file for nothing.
  const stopBtn = document.getElementById("btn-stop");
  if (stopBtn && !stopBtn.disabled) {
    status.textContent = t("status.stop_capture_first");
    status.style.color = "#ef5350";
    return;
  }
  status.textContent = t("status.loading_file", { name: file.name });
  status.style.color = "#90caf9";
  try {
    const buf = await file.arrayBuffer();
    // JSON-array IPC (same pattern as set_tx_source) — Tauri 2 doesn't
    // wire raw-binary arguments by default in this codebase. For long
    // captures (tens of MB) the transfer is the bottleneck; the user
    // sees a "chargement" status while it happens.
    const bytes = Array.from(new Uint8Array(buf));
    const forced = !!currentSettings.rx_force_mode;
    const profile = forced ? (currentSettings.rx_forced_profile || "HIGH") : "HIGH";
    await invoke("start_capture_from_wav", { args: { bytes, profile, forced } });
    status.textContent = t("status.wav_playback", { name: file.name });
    status.style.color = "#ffb74d";
    document.getElementById("btn-start").disabled = true;
    document.getElementById("btn-stop").disabled = false;
    const rxSel = document.getElementById("rx-device-select");
    if (rxSel) rxSel.disabled = true;
    emit("capture:started");
    logEvent("wav_playback_start", { file: file.name, profile, forced });
  } catch (err) {
    status.textContent = t("status.wav_playback_error", { err });
    status.style.color = "#ef5350";
    logEvent("wav_playback_error", { message: String(err) });
  }
}

export async function stopCapture() {
  const status = document.getElementById("status");
  try {
    await invoke("stop_capture");
    status.textContent = t("status.stopped");
    status.style.color = "#9ccc65";
    document.getElementById("btn-stop").disabled = true;
    const rxSel = document.getElementById("rx-device-select");
    if (rxSel) rxSel.disabled = false;
    // RX stopped: tabs refresh the start button / warn-bar / duplex bar /
    // realtime chip / raw-record state.
    emit("capture:stopped");
    logEvent("stop", null);
  } catch (err) {
    status.textContent = t("status.error_stop", { err });
    status.style.color = "#ef5350";
  }
}
