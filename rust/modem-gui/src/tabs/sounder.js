// Sounder tab — channel sounding: emit a multitone/chirp/AWGN probe sequence,
// capture + analyze the RX chain response, plot it (SVG), and POST a report to
// the collector. Self-contained; the TX-emit and RX-regenerate paths share
// buildStandardSoundingRequest internally.
import { invoke, openExternalUrl } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "../lib/state.js";
import { getSelectedBackendId } from "../lib/dom.js";
import { now, fmtNumOrDash } from "../lib/format.js";

export const SOUNDER_LEVELS_DBFS = [
  -30, -27, -24, -21, -18, -15, -12, -9, -6, -3, 0,
];

export function levelDbToAmp(level_db) {
  return Math.pow(10, level_db / 20);
}

export function expandLevels(factory) {
  const probes = [];
  const levels = [];
  for (const level_db of SOUNDER_LEVELS_DBFS) {
    probes.push(factory(levelDbToAmp(level_db)));
    levels.push(level_db);
  }
  return { probes, levels };
}

export function defaultSounderProbes() {
  const probes = [];
  const levels = [];
  // 1 — Level sweep on tone @ 1500 Hz. Runs FIRST so the analyser
  //     can pin down the sweet spot before evaluating the other
  //     probe families. Each family is then evaluated at the same
  //     11 levels so the analyser can chart degradation against
  //     TX level (and flag over-modulation explicitly).
  probes.push({
    kind: "level_sweep",
    inner_tone_freq_hz: 1500.0,
    levels_db: SOUNDER_LEVELS_DBFS,
    duration_s_per_level: 0.6,
    gap_s: 0.15,
  });
  levels.push(null);
  const families = [
    // 2 — Two-tone IMD3. f1=1300/f2=1700 so the IMD3 bins
    //     (2·f1−f2=900 Hz, 2·f2−f1=2100 Hz) land inside the clean
    //     part of the NBFM audio band.
    (amp) => ({
      kind: "two_tone",
      f1_hz: 1300.0,
      f2_hz: 1700.0,
      amp_each: 0.5 * amp,
    }),
    // 3 — Linear chirp (group delay, BW).
    (amp) => ({
      kind: "chirp_linear",
      f0_hz: 200.0,
      f1_hz: 2600.0,
      amplitude: 0.9 * amp,
    }),
    // 4 — Multitone (frequency response).
    (amp) => ({
      kind: "multitone",
      freqs_hz: [300, 600, 900, 1200, 1500, 1800, 2100, 2400],
      amp_each: 0.3 * amp,
    }),
    // 5 — AWGN (noise-floor shape under AGC).
    (amp) => ({
      kind: "awgn",
      rms: 0.2 * amp,
      seed: 0xc0ffee,
    }),
    // 6 — Golay complementary-pair impulse response (high-BW). BPSK on
    //     1500 Hz carrier, 256 chips at 1200 chip/s → ≈ 213 ms per
    //     sequence × 2 + 100 ms gap × 2 ≈ 0.63 s / level instance.
    (amp) => ({
      kind: "golay_pair",
      length_bits: 256,
      chip_rate_hz: 1200.0,
      carrier_hz: 1500.0,
      amplitude: 0.7 * amp,
      gap_s: 0.1,
    }),
    // 7 — Golay complementary-pair impulse response (low-BW). Same
    //     carrier + length as #6 but half the chip rate (600 chip/s),
    //     which doubles the IR mainlobe width. Pairing the two BWs at
    //     the analyser lets us split true multipath from OTA-chain
    //     group-delay smear (see `sounder.rs` dual-BW Golay solve).
    //     ≈ 427 ms per sequence × 2 + 100 ms gap × 2 ≈ 1.05 s / level.
    (amp) => ({
      kind: "golay_pair",
      length_bits: 256,
      chip_rate_hz: 600.0,
      carrier_hz: 1500.0,
      amplitude: 0.7 * amp,
      gap_s: 0.1,
    }),
  ];
  for (const factory of families) {
    const ex = expandLevels(factory);
    probes.push(...ex.probes);
    levels.push(...ex.levels);
  }
  return { probes, levels };
}

export function setSounderStatus(id, text, level) {
  const el = document.getElementById(id);
  if (!el) return;
  el.textContent = text;
  el.classList.remove("ok", "err");
  if (level === "ok") el.classList.add("ok");
  else if (level === "err") el.classList.add("err");
}

export function buildStandardSoundingRequest() {
  const { probes, levels } = defaultSounderProbes();
  return {
    channel_family: "fm",
    probes,
    probe_levels_db: levels,
    wake_up_amplitude: 0.6,
    sync_marker_amplitude: 0.7,
    // 0.4 s per non-sweep probe instance keeps 56 segments × 0.4 ≈
    // 22 s of probe airtime in the new ramp-first schedule. The
    // level sweep (probe #1) has its own duration_s_per_level (0.6).
    default_probe_duration_s: 0.4,
    // 0.1 s inter-probe gap reduces total gap airtime to ~5.6 s
    // across the 56 segments — enough for AGC settle between probes
    // but avoiding the 16 s we'd get with the legacy 0.3 s default.
    inter_probe_gap_s: 0.1,
    metadata: {
      tx_callsign: "",
      rx_callsign: "",
      equipment: "",
      notes: "",
      // Deterministic timestamp (0) so the schedule stays
      // byte-identical between machines that run the sounder at
      // different wall-clock times; metadata.ts_unix is only used
      // to label the signature on the RX side anyway.
      ts_unix: 0,
    },
  };
}

export async function runSounderTxEmit() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  // Read the device from the persisted settings (Paramètres tab).
  // The previous version queried `getElementById("tx-device")` which
  // never existed in the DOM (the actual id is `tx-device-select` and
  // it lives under Paramètres), so the button always errored out
  // even with a device configured.
  const txDevice = ((currentSettings && currentSettings.tx_device) || "").trim();
  if (!txDevice) {
    setSounderStatus(
      "sounder-tx-status",
      t("sounder.tx_pick_in_settings"),
      "err",
    );
    return;
  }
  const btn = document.getElementById("sounder-tx-emit");
  const oldText = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = t("sounder.tx_emitting");
  }
  setSounderStatus("sounder-tx-status", t("sounder.preparing"));
  try {
    const request = buildStandardSoundingRequest();
    const res = await invoke("sounding_tx_emit", {
      args: { request, tx_device: txDevice },
    });
    // Update the duration estimate next to the helper text.
    const est = document.getElementById("sounder-tx-duration-est");
    if (est) est.textContent = res.duration_s.toFixed(0);
    setSounderStatus(
      "sounder-tx-status",
      t("sounder.emitting_s", { s: res.duration_s.toFixed(0) }),
    );
    // Re-enable the button after the airtime + a small safety margin.
    const reenableMs = Math.ceil(res.duration_s * 1000) + 500;
    setTimeout(() => {
      if (btn) {
        btn.disabled = false;
        btn.textContent = oldText ?? t("channel.sounder_tx_emit");
      }
      setSounderStatus("sounder-tx-status", t("sounder.done_at", { time: now() }), "ok");
    }, reenableMs);
  } catch (err) {
    if (btn) {
      btn.disabled = false;
      btn.textContent = oldText ?? t("channel.sounder_tx_emit");
    }
    setSounderStatus("sounder-tx-status", t("status.error_prefix", { err }), "err");
  }
}

export async function runSounderTxRender() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  setSounderStatus("sounder-an-status", t("sounder.generating"));
  try {
    const request = buildStandardSoundingRequest();
    const res = await invoke("sounding_tx_render", { request });
    const sched = document.getElementById("sounder-an-schedule");
    if (sched) sched.value = res.schedule_json;
    setSounderStatus(
      "sounder-an-status",
      t("sounder.ref_regenerated", { s: res.duration_s.toFixed(0) }),
      "ok",
    );
  } catch (err) {
    setSounderStatus("sounder-an-status", t("status.error_prefix", { err }), "err");
  }
}

export function buildRxChainMetadata() {
  const txModel = (
    document.getElementById("sounder-rx-tx-model")?.value || ""
  ).trim();
  const rxModel = (
    document.getElementById("sounder-rx-rx-model")?.value || ""
  ).trim();
  // The TX station's Maidenhead grid is dictated by phonie from the
  // remote operator (the local locator lives in Paramètres). It's
  // optional — leave empty if not communicated.
  const txLocator = (
    document.getElementById("sounder-rx-tx-locator")?.value || ""
  ).trim();
  const relay = (
    document.getElementById("sounder-rx-relay")?.value || "none"
  ).trim();
  const mode = (
    document.getElementById("sounder-rx-mode")?.value || "fm_5khz"
  ).trim();
  // Compact human-readable equipment string for the in-signature
  // `SoundingMetadata` (legacy flat schema).
  const eqParts = [];
  if (txModel) eqParts.push(`TX=${txModel}`);
  if (rxModel) eqParts.push(`RX=${rxModel}`);
  const equipment = eqParts.join(" / ");
  const notes = `relay=${relay} mode=${mode}`;
  // Also surface the individual fields so the collector-submit code
  // can map them onto the structured `ReportMeta` (tx_model, relay,
  // profile) the server expects. `txLocator` rides alongside as an
  // extra `tx_locator` field — the server doesn't parse it today but
  // preserves it in the on-disk metadata.json for later indexing.
  return { equipment, notes, txModel, rxModel, txLocator, relay, mode };
}

export async function runSounderAnalyze() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const capture = (
    document.getElementById("sounder-an-capture")?.value || ""
  ).trim();
  let schedule = (
    document.getElementById("sounder-an-schedule")?.value || ""
  ).trim();
  if (!capture) {
    setSounderStatus("sounder-an-status", t("sounder.no_capture"), "err");
    return;
  }
  // Auto-regenerate the reference schedule if the user didn't already
  // produce one. The generator is deterministic so re-running on the
  // RX side gives bit-identical bytes to whatever the TX side built.
  if (!schedule) {
    setSounderStatus("sounder-an-status", t("sounder.gen_ref"));
    try {
      const request = buildStandardSoundingRequest();
      const ref = await invoke("sounding_tx_render", { request });
      schedule = ref.schedule_json;
      const sched = document.getElementById("sounder-an-schedule");
      if (sched) sched.value = schedule;
    } catch (err) {
      setSounderStatus("sounder-an-status", t("status.error_prefix", { err }), "err");
      return;
    }
  }
  const threshold =
    Number(document.getElementById("sounder-an-threshold")?.value) || 6;
  const { equipment, notes } = buildRxChainMetadata();
  setSounderStatus("sounder-an-status", t("sounder.analyzing"));
  try {
    const sig = await invoke("sounding_analyze", {
      captureWav: capture,
      scheduleJson: schedule,
      family: "fm",
      metadata: {
        tx_callsign: "",
        rx_callsign: "",
        equipment,
        notes,
        ts_unix: Math.floor(Date.now() / 1000),
      },
      syncThreshold: threshold,
    });
    const d = sig.derived || {};
    document.getElementById("sd-snr").textContent = fmtNumOrDash(d.snr_est_db, 2);
    document.getElementById("sd-ip3").textContent = fmtNumOrDash(d.ip3_dbfs, 2);
    document.getElementById("sd-p1db").textContent = fmtNumOrDash(d.p1db_dbfs, 2);
    document.getElementById("sd-sweet").textContent =
      fmtNumOrDash(d.sweet_spot_dbfs, 2);
    if (d.bw_3db_hz && d.bw_3db_hz[1] > 0) {
      document.getElementById("sd-bw").textContent = `${Math.round(
        d.bw_3db_hz[0],
      )}–${Math.round(d.bw_3db_hz[1])}`;
    } else {
      document.getElementById("sd-bw").textContent = "—";
    }
    document.getElementById("sd-gd").textContent = fmtNumOrDash(
      d.group_delay_peak_us,
      0,
    );
    document.getElementById("sd-noise").textContent = fmtNumOrDash(
      d.noise_floor_dbfs,
      2,
    );
    document.getElementById("sd-d50").textContent = fmtNumOrDash(
      d.delay_spread_50_us,
      0,
    );
    document.getElementById("sd-d90").textContent = fmtNumOrDash(
      d.delay_spread_90_us,
      0,
    );
    document.getElementById("sd-mp50").textContent = fmtNumOrDash(
      d.multipath_delay_50_us,
      0,
    );
    document.getElementById("sd-mp90").textContent = fmtNumOrDash(
      d.multipath_delay_90_us,
      0,
    );
    document.getElementById("sd-smear").textContent = fmtNumOrDash(
      d.ota_smear_us,
      0,
    );
    document.getElementById("sd-echo").textContent = fmtNumOrDash(
      d.strongest_echo_dbc,
      1,
    );
    document.getElementById("sd-anchor").textContent =
      sig.capture_anchor_sample != null
        ? String(sig.capture_anchor_sample)
        : "—";
    document.getElementById("sounder-an-signature-path").textContent =
      t("sounder.signature_written", { n: sig.measurements?.length ?? 0 });
    // Verdict from the over-modulation analyser.
    const v = sig.verdict || {};
    const vEl = document.getElementById("sd-verdict");
    if (vEl) {
      vEl.textContent = v.message || "—";
      vEl.classList.remove("ok", "warn");
      if ((v.message || "").includes("OK")) vEl.classList.add("ok");
      else if ((v.message || "").includes("⚠️")) vEl.classList.add("warn");
    }
    // Recommended attenuation to dictate over the air to the TX
    // operator. The sweet_spot_dbfs is the peak level (relative to
    // full-scale) at which the rig stops over-modulating; the TX
    // operator types that same negative value into Canal → Cascade.
    // Round to the nearest integer dB because that's what gets
    // dictated by voice (and matches the cascade slider's effective
    // resolution after the recent fix).
    const recoEl = document.getElementById("sd-reco-att");
    if (recoEl) {
      const sweet = Number(v.sweet_spot_dbfs);
      if (Number.isFinite(sweet)) {
        // Clamp to [-30, 0] like the TX attenuation chain itself.
        const clamped = Math.min(0, Math.max(-30, sweet));
        recoEl.textContent = String(Math.round(clamped));
      } else {
        recoEl.textContent = "—";
      }
    }
    // Stash the result so the collector-send button can pick it up
    // later without re-invoking the analyser. Keep both the raw form
    // fields (mapped to the collector's ReportMeta schema) and a
    // free-text `notes` line that carries the rx_model — for which
    // the server schema has no dedicated field yet.
    const chain = buildRxChainMetadata();
    const noteParts = [];
    if (chain.rxModel) noteParts.push(`RX=${chain.rxModel}`);
    if (chain.mode && chain.mode !== "fm_5khz") {
      noteParts.push(`mode=${chain.mode}`);
    }
    lastSounderResult = {
      wavPath: capture,
      signatureJson: JSON.stringify(sig),
      txModel: chain.txModel || null,
      txLocator: chain.txLocator || null,
      relay: chain.relay && chain.relay !== "none" ? chain.relay : null,
      profile: chain.mode || null,
      notes: noteParts.length ? noteParts.join(" / ") : null,
    };
    const sendBtn = document.getElementById("sd-collector-send");
    if (sendBtn) sendBtn.disabled = false;
    const sendStatus = document.getElementById("sd-collector-status");
    if (sendStatus) {
      sendStatus.textContent = "";
      sendStatus.classList.remove("ok", "err");
    }
    const res = document.getElementById("sounder-an-results");
    if (res) res.hidden = false;
    // Render the channel-characterisation plots from the per-segment
    // measurements that are now in the signature.
    try {
      renderSounderPlots(sig);
    } catch (plotErr) {
      console.error("renderSounderPlots", plotErr);
    }
    setSounderStatus("sounder-an-status", `OK ${now()}`, "ok");
  } catch (err) {
    setSounderStatus("sounder-an-status", `erreur : ${err}`, "err");
  }
}

export let lastSounderResult = null;

export async function runSounderCollectorSend() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  if (!lastSounderResult) return;
  const btn = document.getElementById("sd-collector-send");
  const statusEl = document.getElementById("sd-collector-status");
  const wavChk = document.getElementById("sd-collector-with-wav");
  const setStatus = (msg, kind) => {
    if (!statusEl) return;
    statusEl.textContent = msg;
    statusEl.classList.remove("ok", "err");
    if (kind) statusEl.classList.add(kind);
  };
  const callsign = (currentSettings && currentSettings.callsign || "").trim();
  if (!callsign) {
    setStatus(t("status.callsign_empty"), "err");
    return;
  }
  const collectorUrl =
    (currentSettings && currentSettings.collector_url || "").trim();
  if (!collectorUrl) {
    setStatus(t("status.collector_url_empty"), "err");
    return;
  }
  if (btn) btn.disabled = true;
  setStatus("envoi en cours…");
  try {
    const locator = (currentSettings && currentSettings.locator || "").trim();
    const res = await invoke("submit_sounding", {
      args: {
        wav_path: lastSounderResult.wavPath || "",
        callsign,
        collector_url: collectorUrl,
        signature_json: lastSounderResult.signatureJson,
        locator: locator || null,
        tx_locator: lastSounderResult.txLocator || null,
        profile: lastSounderResult.profile || null,
        relay: lastSounderResult.relay || null,
        tx_model: lastSounderResult.txModel || null,
        notes: lastSounderResult.notes || null,
        include_wav: !!(wavChk && wavChk.checked),
      },
    });
    const link = `${collectorUrl.replace(/\/+$/, "")}${res.url || ""}`;
    setStatus(`OK ${(res.bytes_uploaded / 1024).toFixed(0)} KiB — ${link}`, "ok");
    // Best-effort: open the entry in the OS browser. The opener
    // plugin is optional; swallow the error if it's not registered.
    try {
      await openExternalUrl(link);
    } catch (_) {
      /* the URL is shown in the status line anyway */
    }
  } catch (err) {
    setStatus(`erreur : ${err}`, "err");
  } finally {
    if (btn) btn.disabled = false;
  }
}

export let sounderRxRecording = false;

export let sounderRxLiveTap = false;

export async function runSounderRxCaptureToggle() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const btn = document.getElementById("sounder-rx-capture-toggle");
  const analyseBtn = document.getElementById("sounder-an-run");
  if (!sounderRxRecording) {
    // Reuse the RX device the user has already configured in
    // Paramètres — same dropdown as the main RX play button.
    const deviceSelect = document.getElementById("rx-device-select");
    const deviceName = deviceSelect ? deviceSelect.value : "";
    // If an SDR receiver is already running, don't reopen the device (it's
    // exclusive) — tap its live audio via the raw-capture tee instead, so
    // reception keeps going while we sound the channel.
    const rxStopBtn = document.getElementById("btn-stop");
    const rxRunning = rxStopBtn && !rxStopBtn.disabled;
    const tapLive = rxRunning && getSelectedBackendId("rx-device-select") !== null;
    if (!tapLive && !deviceName) {
      setSounderStatus(
        "sounder-an-status",
        t("sounder.pick_rx_card"),
        "err",
      );
      return;
    }
    try {
      const path = tapLive
        ? await invoke("start_raw_recording")
        : await invoke("sounding_rx_start_capture", { deviceName });
      sounderRxLiveTap = tapLive;
      sounderRxRecording = true;
      const captureInput = document.getElementById("sounder-an-capture");
      if (captureInput) captureInput.value = path;
      if (btn) {
        btn.textContent = t("channel.sounder_capture_stop");
        btn.classList.add("recording");
      }
      if (analyseBtn) analyseBtn.disabled = true;
      setSounderStatus(
        "sounder-an-status",
        t("sounder.capture_in_progress"),
      );
    } catch (err) {
      setSounderStatus("sounder-an-status", t("status.error_prefix", { err }), "err");
    }
  } else {
    try {
      // Match the stop to how we started: raw-capture tee vs standalone
      // device capture. Both return { path, duration_sec }.
      const info = sounderRxLiveTap
        ? await invoke("stop_raw_recording")
        : await invoke("sounding_rx_stop_capture");
      sounderRxRecording = false;
      const captureInput = document.getElementById("sounder-an-capture");
      if (captureInput) captureInput.value = info.path;
      if (btn) {
        btn.textContent = t("channel.sounder_capture_start");
        btn.classList.remove("recording");
      }
      if (analyseBtn) analyseBtn.disabled = false;
      setSounderStatus(
        "sounder-an-status",
        t("sounder.capture_analyzing", { s: info.duration_sec.toFixed(0) }),
      );
      // Auto-fire the analyser. The function regenerates the
      // reference schedule itself if the field is empty.
      await runSounderAnalyze();
    } catch (err) {
      setSounderStatus("sounder-an-status", `erreur stop : ${err}`, "err");
    }
  }
}

export function svgXY(svgId, points, opts) {
  const svg = document.getElementById(svgId);
  if (!svg) return;
  while (svg.firstChild) svg.removeChild(svg.firstChild);
  if (!points || points.length === 0) return;
  const o = Object.assign(
    {
      w: 360,
      h: 220,
      padL: 38,
      padR: 12,
      padT: 12,
      padB: 28,
      xMin: null,
      xMax: null,
      yMin: null,
      yMax: null,
      xLabel: "",
      yLabel: "",
      diagonal: false,
      sweet: null,
      secondary: null, // [[x, y], …]
    },
    opts,
  );
  let xMin = o.xMin,
    xMax = o.xMax,
    yMin = o.yMin,
    yMax = o.yMax;
  if (xMin === null) xMin = Math.min(...points.map((p) => p[0]));
  if (xMax === null) xMax = Math.max(...points.map((p) => p[0]));
  if (yMin === null) yMin = Math.min(...points.map((p) => p[1]));
  if (yMax === null) yMax = Math.max(...points.map((p) => p[1]));
  if (o.secondary && o.secondary.length) {
    yMin = Math.min(yMin, ...o.secondary.map((p) => p[1]));
    yMax = Math.max(yMax, ...o.secondary.map((p) => p[1]));
  }
  if (xMin === xMax) xMax = xMin + 1;
  if (yMin === yMax) yMax = yMin + 1;
  // Add 5% margin so traces don't kiss the axes.
  const dy = yMax - yMin;
  yMin -= dy * 0.05;
  yMax += dy * 0.05;

  const plotW = o.w - o.padL - o.padR;
  const plotH = o.h - o.padT - o.padB;
  const sx = (x) => o.padL + ((x - xMin) / (xMax - xMin)) * plotW;
  const sy = (y) => o.padT + (1 - (y - yMin) / (yMax - yMin)) * plotH;

  const NS = "http://www.w3.org/2000/svg";
  const mk = (tag, attrs, text) => {
    const el = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
    if (text != null) el.textContent = text;
    return el;
  };

  // Grid + axes (5 vertical, 4 horizontal ticks).
  for (let i = 0; i <= 5; i++) {
    const x = sx(xMin + (i / 5) * (xMax - xMin));
    svg.appendChild(
      mk("line", { class: "grid", x1: x, y1: o.padT, x2: x, y2: o.padT + plotH }),
    );
    svg.appendChild(
      mk(
        "text",
        { class: "label", x: x, y: o.padT + plotH + 12, "text-anchor": "middle" },
        (xMin + (i / 5) * (xMax - xMin)).toFixed(
          Math.abs(xMax - xMin) > 50 ? 0 : 1,
        ),
      ),
    );
  }
  for (let i = 0; i <= 4; i++) {
    const y = sy(yMin + (i / 4) * (yMax - yMin));
    svg.appendChild(
      mk("line", { class: "grid", x1: o.padL, y1: y, x2: o.padL + plotW, y2: y }),
    );
    svg.appendChild(
      mk(
        "text",
        { class: "label", x: o.padL - 4, y: y + 3, "text-anchor": "end" },
        (yMin + (i / 4) * (yMax - yMin)).toFixed(
          Math.abs(yMax - yMin) > 50 ? 0 : 1,
        ),
      ),
    );
  }
  svg.appendChild(
    mk("line", {
      class: "axis",
      x1: o.padL,
      y1: o.padT,
      x2: o.padL,
      y2: o.padT + plotH,
    }),
  );
  svg.appendChild(
    mk("line", {
      class: "axis",
      x1: o.padL,
      y1: o.padT + plotH,
      x2: o.padL + plotW,
      y2: o.padT + plotH,
    }),
  );
  if (o.xLabel)
    svg.appendChild(
      mk(
        "text",
        {
          class: "axis-title",
          x: o.padL + plotW / 2,
          y: o.h - 4,
          "text-anchor": "middle",
        },
        o.xLabel,
      ),
    );
  if (o.yLabel)
    svg.appendChild(
      mk(
        "text",
        {
          class: "axis-title",
          x: 9,
          y: o.padT + plotH / 2,
          "text-anchor": "middle",
          transform: `rotate(-90 9 ${o.padT + plotH / 2})`,
        },
        o.yLabel,
      ),
    );

  if (o.diagonal) {
    // y=x reference (only meaningful when both axes have the same units).
    const xa = Math.max(xMin, yMin);
    const xb = Math.min(xMax, yMax);
    if (xb > xa) {
      svg.appendChild(
        mk("line", {
          class: "ref",
          x1: sx(xa),
          y1: sy(xa),
          x2: sx(xb),
          y2: sy(xb),
        }),
      );
    }
  }

  if (o.sweet != null && isFinite(o.sweet) && o.sweet >= xMin && o.sweet <= xMax) {
    const xs = sx(o.sweet);
    svg.appendChild(
      mk("line", {
        class: "sweet",
        x1: xs,
        y1: o.padT,
        x2: xs,
        y2: o.padT + plotH,
      }),
    );
    svg.appendChild(
      mk(
        "text",
        {
          class: "sweet-label",
          x: xs + 3,
          y: o.padT + 10,
          "text-anchor": "start",
        },
        "sweet",
      ),
    );
  }

  // Secondary trace first so it sits BEHIND the main trace.
  if (o.secondary && o.secondary.length > 1) {
    const d = o.secondary
      .map((p, i) => `${i === 0 ? "M" : "L"} ${sx(p[0])} ${sy(p[1])}`)
      .join(" ");
    svg.appendChild(mk("path", { class: "trace-secondary", d }));
  }
  const d = points
    .map((p, i) => `${i === 0 ? "M" : "L"} ${sx(p[0])} ${sy(p[1])}`)
    .join(" ");
  svg.appendChild(mk("path", { class: "trace", d }));
  // Add small dots so sparse data is visible.
  if (points.length <= 32) {
    for (const p of points) {
      svg.appendChild(
        mk("circle", { class: "point", cx: sx(p[0]), cy: sy(p[1]), r: 2 }),
      );
    }
  }
}

export function renderSounderPlots(sig) {
  const meas = sig.measurements || [];
  const d = sig.derived || {};
  const sweet = d.sweet_spot_dbfs;

  // (1) AM-AM — from the unique LevelSweep entry.
  const sweep = meas.find((m) => m.kind === "level_sweep");
  if (sweep && sweep.result && sweep.result.am_am_curve) {
    svgXY("sd-plot-amam", sweep.result.am_am_curve, {
      xLabel: t("channel.axis_input_dbfs"),
      yLabel: t("channel.axis_output_dbfs"),
      diagonal: true,
      sweet,
    });
    // (1bis) AM-PM curve overlay onto the AM-PM panel.
    if (sweep.result.am_pm_curve) {
      svgXY("sd-plot-ampm", sweep.result.am_pm_curve, {
        xLabel: t("channel.axis_input_dbfs"),
        yLabel: t("channel.axis_phase_rad"),
        sweet,
      });
    }
  }

  // (2) IMD3 vs level — group all two-tone measurements with their
  //     parent ProbeSegment's level_db (from the schedule, which the
  //     analyse pass doesn't echo back, so we reverse-engineer from
  //     amp_each: level = 20·log10(amp_each / 0.5)).
  //
  //     We don't have the schedule here, but we have measurements with
  //     amp inside each spec's result. The two-tone measurements
  //     return a1_dbfs / a2_dbfs (output level), and we can map
  //     measurement index back to its input level if the schedule was
  //     standard. As a pragmatic shortcut, use a1_dbfs as the x-axis
  //     proxy (output level at the receiver — what actually matters
  //     for IMD3 anyway).
  const tts = meas
    .filter((m) => m.kind === "two_tone")
    .map((m) => [m.result.a1_dbfs, 0.5 * (m.result.imd3_low_dbc + m.result.imd3_high_dbc)])
    .sort((a, b) => a[0] - b[0]);
  if (tts.length > 0) {
    svgXY("sd-plot-imd3", tts, {
      xLabel: t("channel.axis_out_f1_dbfs"),
      yLabel: t("channel.axis_imd3_dbc"),
    });
  }

  // (3) Frequency response — from the multitone at sweet-spot.
  let mtPick = null;
  let mtBestDist = Infinity;
  for (const m of meas) {
    if (m.kind !== "multitone") continue;
    // We don't carry level_db on the measurement, so we approximate
    // "sweet-spot multitone" as the one whose strongest bin is closest
    // to sweet_spot in dBFS. The peak bin amp is the multitone result's
    // ref level (gain_db_per_freq is normalised to peak).
    // Practical fallback: pick the highest output level (= highest
    // measured raw multitone peak amp ≈ middle-of-band tone amplitude).
    // We just look at the first gain entry which is normalised to 0 dB
    // at peak, so all multitones look similar shape-wise. Pick the
    // last one (highest TX level) for the curve display.
    mtPick = m;
  }
  if (mtPick && mtPick.result && mtPick.result.gain_db_per_freq) {
    svgXY("sd-plot-freq", mtPick.result.gain_db_per_freq, {
      xLabel: t("channel.axis_freq_hz"),
      yLabel: t("channel.axis_gain_db"),
    });
  }

  // (4) Group delay — from chirp at sweet-spot. Take the last chirp
  //     (highest level, best SNR for the IF estimator).
  let chPick = null;
  for (const m of meas) {
    if (m.kind === "chirp") chPick = m;
  }
  if (chPick && chPick.result && chPick.result.group_delay_per_freq) {
    svgXY("sd-plot-gd", chPick.result.group_delay_per_freq, {
      xLabel: t("channel.axis_freq_hz"),
      yLabel: t("channel.axis_delta_gd_us"),
    });
  }

  // (5) Impulse response — pick the Golay with the highest peak amp
  //     (same heuristic as the derived numbers).
  let irPick = null;
  let irBest = -Infinity;
  for (const m of meas) {
    if (m.kind !== "golay_pair") continue;
    if (m.result && m.result.peak_amplitude > irBest) {
      irBest = m.result.peak_amplitude;
      irPick = m;
    }
  }
  if (irPick && irPick.result && irPick.result.impulse_response) {
    const ir = irPick.result.impulse_response;
    const peak = irPick.result.peak_amplitude || 1;
    // Show in dBc relative to peak. Sample down to ~120 points so
    // SVG stays light.
    const stride = Math.max(1, Math.floor(ir.length / 120));
    const pts = [];
    for (let i = 0; i < ir.length; i += stride) {
      const v = ir[i];
      const dbc = 20 * Math.log10(Math.max(v / peak, 1e-6));
      pts.push([(i / 48), dbc]); // x in ms (48k -> 48 samples per ms)
    }
    svgXY("sd-plot-ir", pts, {
      xLabel: t("channel.axis_delay_ms"),
      yLabel: t("channel.axis_abs_h_dbc"),
      yMin: -40,
      yMax: 3,
    });
  }
}

export function setupSounderTab() {
  document
    .getElementById("sounder-tx-emit")
    ?.addEventListener("click", runSounderTxEmit);
  document
    .getElementById("sounder-rx-capture-toggle")
    ?.addEventListener("click", runSounderRxCaptureToggle);
  document
    .getElementById("sounder-an-run")
    ?.addEventListener("click", runSounderAnalyze);
  // Advanced: regenerate the reference schedule manually (the
  // analyse-side does it automatically when the field is blank, but
  // power users may want to ferry the file or inspect it).
  document
    .getElementById("sounder-an-regen-ref")
    ?.addEventListener("click", runSounderTxRender);
  document
    .getElementById("sd-collector-send")
    ?.addEventListener("click", runSounderCollectorSend);
}
