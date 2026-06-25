// RX tab — received-file panel, audio-level meter, modem-state chip, the
// RaptorQ progress bitmap / constellation / pilot-phase renderers, fountain
// status, raw-WAV recording, the realtime-margin chip, and the Phase-D
// capture-submit prompt. Owns lastProgress (the #progress-blocks bitmap); TX
// half-duplex repaints it through the exported setProgressBitmap setter.
import { invoke, listen, convertFileSrc } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "../lib/state.js";
import { logEvent, eventLogBuffer } from "../lib/log.js";
import { now, numOr, fmtSeconds, formatBytes, formatTimestamp, fmtNumOrDash, isImageMime, mimeToExt, MIME_TYPES, escapeHtml } from "../lib/format.js";
import { openLightbox } from "../lib/lightbox.js";
import { rxIsRunning, getSelectedBackendId } from "../lib/dom.js";

export function showCurrentFile(payload) {
  const info = document.getElementById("current-info");
  const wrap = document.getElementById("current-image-wrap");
  const mime = MIME_TYPES[payload.mime_type] || "application/octet-stream";
  // σ² shown in the file panel is the running mean over every decode
  // tick that contributed to the session (data-symbol residuals only,
  // pilots/preamble excluded). We label it as such and surface the
  // implied SNR in dB. Falls back to the legacy single-window σ² if
  // the worker isn't emitting the new field yet.
  const sigAvg = Number.isFinite(payload.sigma2_data_avg)
    ? payload.sigma2_data_avg
    : payload.sigma2;
  const sigStr = Number.isFinite(sigAvg) ? sigAvg.toFixed(4) : "—";
  const snrStr = Number.isFinite(sigAvg) && sigAvg > 0
    ? `${(-10 * Math.log10(sigAvg)).toFixed(1)} dB`
    : "— dB";
  info.innerHTML =
    `<strong>De :</strong> ${payload.callsign || "?"} · ` +
    `<strong>Nom :</strong> ${payload.filename} · ` +
    `<strong>Taille :</strong> ${payload.size} o · ` +
    `<strong>MIME :</strong> ${mime} · ` +
    `<strong>σ² moyen :</strong> ${sigStr} (${snrStr}) · ` +
    `<code>${payload.saved_path}</code>`;
  wrap.innerHTML = "";
  if (isImageMime(payload.mime_type)) {
    const src = convertFileSrc(payload.saved_path);
    const img = document.createElement("img");
    img.src = src;
    img.alt = payload.filename;
    img.dataset.src = src;
    img.addEventListener("dblclick", () => openLightbox(src, payload.filename));
    wrap.appendChild(img);
  }
}

export async function revealReceivedFile(savedPath) {
  if (!savedPath) return;
  try {
    const opener = window.__TAURI__ && window.__TAURI__.opener;
    if (opener && typeof opener.revealItemInDir === "function") {
      await opener.revealItemInDir(savedPath);
    } else if (window.__TAURI__ && window.__TAURI__.core) {
      // Fallback through direct invoke if the plugin's global surface is
      // not exposed by withGlobalTauri on this Tauri version.
      await invoke("plugin:opener|reveal_item_in_dir", {
        path: savedPath,
      });
    }
  } catch (err) {
    console.error("revealItemInDir", err);
  }
}

export function refreshRxDeviceLabel() {
  const label = document.getElementById("rx-device-label");
  const select = document.getElementById("rx-device-select");
  if (!label || !select) return;
  const opt = select.options[select.selectedIndex];
  label.textContent = opt && opt.value ? opt.textContent : "— aucune carte RX";
}

export function refreshStartButtonFromRx() {
  const select = document.getElementById("rx-device-select");
  const btn = document.getElementById("btn-start");
  if (!select || !btn) return;
  const opt = select.options[select.selectedIndex];
  const ok = !!(opt && opt.value && opt.dataset.supports48k === "1");
  // Don't touch btn-start if capture is running (disabled via startCapture).
  if (!document.getElementById("btn-stop").disabled) return;
  btn.disabled = !ok;
}

export let rawRecordingActive = false;

export function setRawButtonState(recording) {
  rawRecordingActive = recording;
  const btn = document.getElementById("btn-raw");
  if (recording) {
    btn.classList.add("recording");
    btn.textContent = t("status.stop_capture_btn");
  } else {
    btn.classList.remove("recording");
    btn.textContent = t("rx.raw");
  }
}

export async function refreshRawRecordingState() {
  try {
    const active = await invoke("is_raw_recording");
    setRawButtonState(!!active);
  } catch (err) {
    console.error("is_raw_recording", err);
  }
}

export async function toggleRawRecording() {
  try {
    if (rawRecordingActive) {
      const info = await invoke("stop_raw_recording");
      setRawButtonState(false);
      logEvent("raw_recording_stopped", info);
      maybeOfferCaptureSubmit(info);
    } else {
      const path = await invoke("start_raw_recording");
      setRawButtonState(true);
      logEvent("raw_recording_started", { path });
    }
  } catch (err) {
    logEvent("raw_recording_error", { message: String(err) });
  }
}

export let pendingCapture = null;

export function maybeOfferCaptureSubmit(captureInfo) {
  const url = (currentSettings.collector_url || "").trim();
  const panel = document.getElementById("capture-submit-prompt");
  if (!panel) return;
  if (!url) {
    panel.hidden = true;
    pendingCapture = null;
    return;
  }
  pendingCapture = captureInfo;
  panel.hidden = false;
  panel.classList.remove("busy", "success", "error");
  const meta = document.getElementById("csp-meta");
  if (meta) {
    const sizeMb = (captureInfo.samples * 4 / (1024 * 1024)).toFixed(1);
    meta.textContent = `${captureInfo.duration_sec.toFixed(1)} s · ~${sizeMb} MB · ${captureInfo.path}`;
  }
  const status = document.getElementById("csp-status");
  if (status) status.textContent = t("status.ready_to_submit", { url });
  const submit = document.getElementById("csp-submit");
  const dismiss = document.getElementById("csp-dismiss");
  if (submit) submit.disabled = false;
  if (dismiss) dismiss.disabled = false;
  const notes = document.getElementById("csp-notes");
  if (notes) notes.value = "";
}

export async function submitPendingCapture() {
  if (!pendingCapture) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const panel = document.getElementById("capture-submit-prompt");
  const status = document.getElementById("csp-status");
  const submit = document.getElementById("csp-submit");
  const dismiss = document.getElementById("csp-dismiss");
  const notesEl = document.getElementById("csp-notes");
  const notes = (notesEl && notesEl.value || "").trim() || null;
  if (panel) panel.classList.add("busy");
  if (submit) submit.disabled = true;
  if (dismiss) dismiss.disabled = true;
  if (status) status.textContent = t("status.submitting");
  try {
    const result = await invoke("submit_capture", {
      args: {
        wav_path: pendingCapture.path,
        callsign: currentSettings.callsign || "",
        collector_url: (currentSettings.collector_url || "").trim(),
        profile: currentProfile || null,
        notes,
        event_log_json: JSON.stringify(eventLogBuffer),
      },
    });
    panel.classList.remove("busy");
    panel.classList.add("success");
    const base = (currentSettings.collector_url || "").replace(/\/+$/, "");
    const fullUrl = base + (result.url || "");
    if (status) {
      status.innerHTML = t("status.send_collector_done", { url: escapeHtml(fullUrl), folder: escapeHtml(result.folder) })
        + `(${(result.bytes_uploaded / (1024 * 1024)).toFixed(1)} MB)`;
    }
    if (dismiss) {
      dismiss.disabled = false;
      dismiss.textContent = t("status.close");
    }
    logEvent("capture_submit_ok", { folder: result.folder, bytes: result.bytes_uploaded });
    pendingCapture = null;
  } catch (err) {
    panel.classList.remove("busy");
    panel.classList.add("error");
    if (status) status.textContent = t("status.error_prefix", { err });
    if (submit) submit.disabled = false;
    if (dismiss) dismiss.disabled = false;
    logEvent("capture_submit_error", { message: String(err) });
  }
}

export function dismissCapturePrompt() {
  const panel = document.getElementById("capture-submit-prompt");
  if (panel) {
    panel.hidden = true;
    panel.classList.remove("busy", "success", "error");
  }
  const dismiss = document.getElementById("csp-dismiss");
  if (dismiss) dismiss.textContent = t("rx.dismiss");
  pendingCapture = null;
}

export function updateLevel(rms, peak, _totalSamples) {
  const fill = document.getElementById("level-fill");
  const text = document.getElementById("level-text");
  const db = rms > 1e-6 ? 20 * Math.log10(rms) : -120;
  const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  fill.style.width = `${pct}%`;
  const dbStr = db.toFixed(1).padStart(6, " ");
  const peakStr = peak.toFixed(2).padStart(4, " ");
  text.textContent = `${dbStr} dB · peak ${peakStr}`;
}

export const OVD_STICKY_MS = 5000;

export let lastOverdriveMs = 0;

export let lastCrestDb = NaN;

export function refreshOverdriveChip() {
  const chip = document.getElementById("ovd-chip");
  if (!chip) return;
  const active = lastOverdriveMs > 0 && (Date.now() - lastOverdriveMs) < OVD_STICKY_MS;
  chip.classList.toggle("ovd-on", active);
  chip.classList.toggle("ovd-off", !active);
  if (Number.isFinite(lastCrestDb)) {
    chip.title = `Overdrive TX — crest ${lastCrestDb.toFixed(1)} dB (seuil 8.5 dB)`;
  }
}

export function noteAudioOverdrive(overdrive, crestDb) {
  if (Number.isFinite(crestDb)) lastCrestDb = crestDb;
  if (overdrive) lastOverdriveMs = Date.now();
  refreshOverdriveChip();
}

export const RX_RT_DROP_HOLD_MS = 6000;

export let lastRxRealtime = null;       // last RxRealtimePayload received

export let lastDropTimestamp = 0;       // wall-clock ms of last fresh drop

export let lastSeenDroppedSamples = 0;  // monotonic counter from previous tick

export let rxRealtimeActive = false;    // false until first event after a fresh start

export function refreshRxRealtimeChip() {
  const chip = document.getElementById("rt-chip");
  if (!chip) return;
  if (!rxRealtimeActive || !lastRxRealtime) {
    chip.classList.remove("rt-ok", "rt-warn", "rt-err");
    chip.classList.add("rt-off");
    chip.title = t("rt.inactive");
    return;
  }
  const p = lastRxRealtime;
  const recentDrop = (Date.now() - lastDropTimestamp) < RX_RT_DROP_HOLD_MS;
  let state;
  if (recentDrop || p.lag_ms > 300) {
    state = "err";
  } else if (p.lag_ms > 100 || p.last_batch_ms > 500) {
    state = "warn";
  } else {
    state = "ok";
  }
  chip.classList.remove("rt-off", "rt-ok", "rt-warn", "rt-err");
  chip.classList.add(`rt-${state}`);
  const lines = [
    t("rt.active", { state: state.toUpperCase() }),
    t("rt.lag", { ms: p.lag_ms.toFixed(0) }),
    t("rt.last_batch", { ms: p.last_batch_ms.toFixed(0) }),
    t("rt.peak_2s", { ms: p.max_batch_ms.toFixed(0) }),
    t("rt.session_buf", { ms: p.session_buf_ms.toFixed(0) }),
    t("rt.dropped", { n: p.dropped_samples })
      + (recentDrop ? t("rt.brickwall_suffix") : ""),
  ];
  chip.title = lines.join("\n");
}

export function noteRxRealtime(payload) {
  if (!payload) return;
  if (payload.dropped_samples > lastSeenDroppedSamples) {
    lastDropTimestamp = Date.now();
  }
  lastSeenDroppedSamples = payload.dropped_samples | 0;
  lastRxRealtime = payload;
  rxRealtimeActive = true;
  refreshRxRealtimeChip();
}

export function noteRxRealtimeReset() {
  rxRealtimeActive = false;
  lastRxRealtime = null;
  lastSeenDroppedSamples = 0;
  lastDropTimestamp = 0;
  refreshRxRealtimeChip();
}

export function noteProfileFromHeader(_profileStr) {}

export function updateV2State(state) {
  const chip = document.getElementById("v2-state-chip");
  chip.className = `state-chip state-${state}`;
  chip.textContent = state.replace(/_/g, " ");
  if (state === "idle") {
    document.getElementById("v2-marker-info").textContent = "—";
    // Do NOT clear lastProgress / fountain / constellation / pilot phases
    // here: the worker goes idle every time it loses the preamble, even
    // mid-burst while waiting for a late re-entry. Wiping the stats then
    // would make the operator believe everything is lost. The visuals are
    // cleared instead when a *new* session_id arrives (genuinely new
    // transmission, see the session_armed handler).
    noteProfileFromHeader(null);
  }
}

export function updateV2Marker(payload) {
  const info = document.getElementById("v2-marker-info");
  const kind = payload.is_meta ? "meta" : "data";
  const seg = String(payload.seg_id).padStart(2, " ");
  const esi = String(payload.base_esi).padStart(4, " ");
  info.textContent = `seg=${seg} esi=${esi} ${kind}`;
}

export let lastProgress = {
  bitmap: null,
  expected: 0,
  converged: 0,
  sigma2: null,
};

// TX half-duplex repaints the RX progress bitmap; ESM bindings are read-only
// in tx.js, so it reassigns lastProgress through this setter.
export function setProgressBitmap(p) {
  lastProgress = p;
}

export let lastConstellation = [];

export let lastPilotPhases = [];

export let lastPilotPhaseIsMeta = [];

export function resetRxVisuals() {
  lastProgress = { bitmap: null, expected: 0, converged: 0, sigma2: null };
  lastConstellation = [];
  lastPilotPhases = [];
  lastPilotPhaseIsMeta = [];
  const text = document.getElementById("v2-progress-text");
  if (text) text.textContent = "—";
  hideFountainStatus();
  drawProgressBlocks();
  drawConstellation();
  drawPilotPhase();
}

export function hideFountainStatus() {
  fountainState = { sessionId: null, received: 0, needed: 0, decoded: false, capReached: false };
  const el = document.getElementById("rx-fountain-status");
  if (el) el.hidden = true;
}

export let fountainState = { sessionId: null, received: 0, needed: 0, decoded: false, capReached: false };

export function updateFountainStatus(partial) {
  // Merge : null/undefined fields in `partial` leave the previous value in
  // place. This matters for session_decoded (which may not re-send
  // received / needed) and peek-re-announce paths.
  const next = { ...fountainState };
  for (const [k, v] of Object.entries(partial)) {
    if (v !== null && v !== undefined) next[k] = v;
  }
  fountainState = next;
  const el = document.getElementById("rx-fountain-status");
  const counter = document.getElementById("rx-fountain-counter");
  const pct = document.getElementById("rx-fountain-pct");
  const sess = document.getElementById("rx-fountain-session");
  if (!el || !counter || !pct || !sess) return;
  el.hidden = false;
  const k = next.needed || 0;
  const r = next.received || 0;
  // Don't cap "received" at K - the user is allowed to see they have
  // already swallowed more blocks than the strict minimum (repair
  // included). "Missing" cannot go negative: it's max(0, K - R).
  const missing = Math.max(0, k - r);
  const missingTail = next.decoded
    ? ""
    : missing > 0
    ? t("fountain.missing_n", { n: missing })
    : t("fountain.missing_zero");
  counter.textContent = t("fountain.received_blocks", { r, k, tail: missingTail });
  const pctVal = k > 0 ? Math.min(100, Math.round((r * 100) / k)) : 0;
  pct.textContent = next.decoded
    ? t("fountain.decoded_ok")
    : next.capReached
    ? t("fountain.saturated", { pct: pctVal })
    : `${pctVal} %`;
  if (next.sessionId != null) {
    sess.textContent = t("fountain.session", { id: next.sessionId.toString(16).padStart(8, "0") });
  }
  el.dataset.decoded = next.decoded ? "true" : "false";
}

export function updateV2Progress(payload) {
  // Bitmap may arrive as an Array (JSON) — each byte = 8 consecutive ESIs,
  // LSB-first. We store it as Uint8Array for fast bit tests in the render.
  const bm = payload.converged_bitmap;
  const bitmap = bm
    ? new Uint8Array(bm)
    : new Uint8Array(Math.ceil((payload.blocks_expected || 0) / 8));
  // Prefer `sigma2_data` (frame-only, hard-decision residuals) when the
  // worker provides it. Fall back to `sigma2` (pilot-residual) for
  // older payloads so a partial rebuild doesn't blank the indicator.
  const sigmaInst = Number.isFinite(payload.sigma2_data)
    ? payload.sigma2_data
    : (Number.isFinite(payload.sigma2) ? payload.sigma2 : null);
  lastProgress = {
    bitmap,
    expected: payload.blocks_expected || 0,
    converged: payload.blocks_converged || 0,
    sigma2: sigmaInst,
  };
  lastConstellation = Array.isArray(payload.constellation_sample)
    ? payload.constellation_sample
    : [];
  lastPilotPhases = Array.isArray(payload.pilot_phase_segments)
    ? payload.pilot_phase_segments
    : [];
  lastPilotPhaseIsMeta = Array.isArray(payload.pilot_phase_is_meta)
    ? payload.pilot_phase_is_meta
    : [];

  const sigmaStr = lastProgress.sigma2 != null
    ? lastProgress.sigma2.toFixed(3).padStart(6, " ")
    : "     ?";
  const mini = document.getElementById("v2-progress-text");
  if (mini) {
    const c = String(lastProgress.converged).padStart(3, " ");
    const e = String(lastProgress.expected).padStart(3, " ");
    mini.textContent = `${c}/${e} σ²=${sigmaStr}`;
  }
  drawProgressBlocks();
  drawConstellation();
  drawPilotPhase();
}

export function redrawAll() {
  drawProgressBlocks();
  drawConstellation();
  drawPilotPhase();
}

export function drawProgressBlocks() {
  const canvas = document.getElementById("progress-blocks");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  // Match canvas pixel size to CSS size for crisp lines.
  const rect = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  if (
    canvas.width !== Math.round(rect.width * dpr) ||
    canvas.height !== Math.round(rect.height * dpr)
  ) {
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
  }
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  const { bitmap, expected, converged } = lastProgress;
  if (!expected || expected <= 0) {
    ctx.fillStyle = "#3a1a1a";
    ctx.fillRect(0, 0, w, h);
    return;
  }
  // "Fountain fill" strategy: the RaptorQ code doesn't need to recover
  // the missing ESIs exactly - K total blocks is enough. So we display
  // the actual bitmap (ESI positions effectively received), then we
  // "plug the holes" as soon as `converged` exceeds the number of bits
  // set in [0..expected): ESIs > expected (coming from More or repair)
  // are not lost, they repaint the first red hole.
  const bw = w / expected;
  const slotConverged = new Array(expected).fill(false);
  let filled = 0;
  if (bitmap) {
    for (let i = 0; i < expected; i++) {
      const byte = bitmap[i >> 3] || 0;
      if (((byte >> (i & 7)) & 1) !== 0) {
        slotConverged[i] = true;
        filled++;
      }
    }
  }
  // Surplus = blocks received beyond what the local bitmap can show.
  // Fills the holes from left to right.
  let surplus = Math.max(0, (converged || 0) - filled);
  if (surplus > 0) {
    for (let i = 0; i < expected && surplus > 0; i++) {
      if (!slotConverged[i]) {
        slotConverged[i] = true;
        surplus--;
      }
    }
  }
  for (let i = 0; i < expected; i++) {
    ctx.fillStyle = slotConverged[i] ? "#9ccc65" : "#c62828";
    ctx.fillRect(Math.floor(i * bw), 0, Math.max(1, Math.ceil(bw) - 1), h);
  }
}

export function drawConstellation() {
  const canvas = document.getElementById("constellation-canvas");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  // Subtle grid + axes.
  ctx.strokeStyle = "#2a2a2a";
  ctx.lineWidth = 1;
  for (let i = 1; i < 4; i++) {
    const x = (i * w) / 4;
    const y = (i * h) / 4;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, h);
    ctx.moveTo(0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
  }
  ctx.strokeStyle = "#4a4a4a";
  ctx.beginPath();
  ctx.moveTo(w / 2, 0);
  ctx.lineTo(w / 2, h);
  ctx.moveTo(0, h / 2);
  ctx.lineTo(w, h / 2);
  ctx.stroke();

  const pts = lastConstellation;
  if (!pts.length) return;
  // Scale: constellation points are unit-magnitude-ish post-correction
  // (±1 for QPSK, up to ~1.5 for 16-APSK outer ring). Map ±1.7 to canvas.
  const scale = (Math.min(w, h) / 2) / 1.7;
  ctx.fillStyle = "rgba(129, 212, 250, 0.85)";
  for (const p of pts) {
    const x = w / 2 + p[0] * scale;
    const y = h / 2 - p[1] * scale;
    ctx.beginPath();
    ctx.arc(x, y, 2.2, 0, 2 * Math.PI);
    ctx.fill();
  }
}

export function drawPilotPhase() {
  const canvas = document.getElementById("pilot-phase-canvas");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  // Match canvas pixel size to CSS for crisp lines.
  const rect = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  if (
    canvas.width !== Math.round(rect.width * dpr) ||
    canvas.height !== Math.round(rect.height * dpr)
  ) {
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
  }
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);

  // Reserve a 56-px gutter on the left for the Y-axis labels so the
  // mrad scale stays readable even on a narrow window.
  const gutter = 56 * dpr;
  const plotX0 = gutter;
  const plotW = w - gutter;

  const segments = lastPilotPhases;
  const total = segments.reduce((acc, s) => acc + s.length, 0);
  if (!segments.length || total < 2) {
    ctx.fillStyle = "#1a1a1a";
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = "#888";
    ctx.font = `bold ${13 * dpr}px monospace`;
    ctx.fillText("phase pilote — en attente", 8 * dpr, 18 * dpr);
    return;
  }

  // Re-anchor each segment to its first sample so the plot shows
  // INTRA-SEGMENT drift only (between-segment jumps come from pilot
  // interp restart, which would dominate the y-range).
  const anchored = segments.map((seg) => {
    const a0 = seg[0] || 0;
    return seg.map((p) => p - a0);
  });

  let ymin = Infinity;
  let ymax = -Infinity;
  for (const seg of anchored) {
    for (const v of seg) {
      if (v < ymin) ymin = v;
      if (v > ymax) ymax = v;
    }
  }
  if (!isFinite(ymin) || !isFinite(ymax)) return;
  const span = ymax - ymin;
  // Floor the range so a near-flat trace doesn't get blown up to ±1 mrad
  // and look noisy for nothing. 50 mrad minimum spread.
  const minHalf = 0.05;
  if (span < 2 * minHalf) {
    const center = (ymax + ymin) / 2;
    ymin = center - minHalf;
    ymax = center + minHalf;
  } else {
    const pad = span * 0.15;
    ymin -= pad;
    ymax += pad;
  }
  const yRange = ymax - ymin;

  // Background panel for the plot area.
  ctx.fillStyle = "rgba(0,0,0,0.0)"; // canvas already dark
  ctx.fillRect(plotX0, 0, plotW, h);

  // Y-axis ticks : 5 levels from ymin to ymax with mrad labels.
  ctx.strokeStyle = "#2a2a2a";
  ctx.fillStyle = "#aaa";
  ctx.font = `${11 * dpr}px monospace`;
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  ctx.lineWidth = 1 * dpr;
  const nTicks = 5;
  for (let i = 0; i <= nTicks; i++) {
    const t = i / nTicks;
    const y = h - t * h;
    const valRad = ymin + t * yRange;
    const valMrad = valRad * 1000;
    ctx.beginPath();
    ctx.moveTo(plotX0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
    const lbl = Math.abs(valMrad) < 10
      ? valMrad.toFixed(1)
      : valMrad.toFixed(0);
    ctx.fillText(`${lbl}`, plotX0 - 4 * dpr, y);
  }
  // Y-axis label
  ctx.save();
  ctx.translate(14 * dpr, h / 2);
  ctx.rotate(-Math.PI / 2);
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillStyle = "#888";
  ctx.font = `${11 * dpr}px monospace`;
  ctx.fillText("phase (mrad)", 0, 0);
  ctx.restore();

  // Zero baseline highlighted
  const yZero = h - ((0 - ymin) / yRange) * h;
  if (yZero >= 0 && yZero <= h) {
    ctx.strokeStyle = "#5a5a5a";
    ctx.lineWidth = 1.5 * dpr;
    ctx.beginPath();
    ctx.moveTo(plotX0, yZero);
    ctx.lineTo(w, yZero);
    ctx.stroke();
  }

  // Walk each segment as a polyline. DATA segments alternate blue/orange
  // so the operator can count them and spot where pilot interp restarts;
  // META segments (header replicated, marked by the worker via
  // pilot_phase_is_meta) get a distinct magenta stroke so they're
  // immediately identifiable in the trace — they have a different pilot
  // density / payload structure than DATA segments and would otherwise
  // be invisible after the rework that removed the meta-filter.
  const dataColours = ["rgba(129, 212, 250, 0.95)", "rgba(255, 183, 77, 0.95)"];
  const metaColour = "rgba(236, 64, 122, 0.95)"; // magenta-pink
  let xCursor = 0;
  let dataIdx = 0;
  const pxPerSample = total > 1 ? plotW / (total - 1) : 0;
  for (let s = 0; s < anchored.length; s++) {
    const seg = anchored[s];
    if (seg.length === 0) continue;
    const isMeta = !!(lastPilotPhaseIsMeta && lastPilotPhaseIsMeta[s]);
    if (isMeta) {
      ctx.strokeStyle = metaColour;
    } else {
      ctx.strokeStyle = dataColours[dataIdx % dataColours.length];
      dataIdx += 1;
    }
    ctx.lineWidth = 2 * dpr;
    ctx.beginPath();
    for (let i = 0; i < seg.length; i++) {
      const x = plotX0 + (xCursor + i) * pxPerSample;
      const y = h - ((seg[i] - ymin) / yRange) * h;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
    xCursor += seg.length;
    if (s < anchored.length - 1) {
      const xb = plotX0 + xCursor * pxPerSample;
      ctx.strokeStyle = "#444";
      ctx.lineWidth = 1 * dpr;
      ctx.setLineDash([4 * dpr, 3 * dpr]);
      ctx.beginPath();
      ctx.moveTo(xb, 0);
      ctx.lineTo(xb, h);
      ctx.stroke();
      ctx.setLineDash([]);
    }
  }

  // Header overlay : range, segments count, σ² (if available), implied SNR.
  const rangeMrad = (yRange * 1000).toFixed(0);
  const sigma2 = lastProgress.sigma2;
  let header = `±${(rangeMrad / 2).toFixed(0)} mrad · ${segments.length} seg`;
  if (sigma2 != null && sigma2 > 0) {
    const snrDb = (-10 * Math.log10(sigma2)).toFixed(1);
    header += ` · σ²=${sigma2.toFixed(3)} (${snrDb} dB)`;
  }
  ctx.fillStyle = "rgba(0,0,0,0.55)";
  ctx.fillRect(plotX0, 0, plotW, 22 * dpr);
  ctx.fillStyle = "#e0e0e0";
  ctx.font = `bold ${12 * dpr}px monospace`;
  ctx.textAlign = "left";
  ctx.textBaseline = "top";
  ctx.fillText(header, plotX0 + 6 * dpr, 4 * dpr);
}
