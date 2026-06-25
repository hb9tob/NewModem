// Channel tab — attenuation cascade (the manual ATT chain + its mean/median
// stats). Self-contained; reads/mutates currentSettings, invokes set_attenuation.
import { now } from "../lib/format.js";
import { invoke } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "../lib/state.js";

export let cascadeFeedback = [];

export function attGainStr(db) {
  const lin = Math.pow(10, db / 20);
  return `×${lin.toFixed(3)} (${db.toFixed(1)} dB)`;
}

export function clampAttDb(v) {
  if (!Number.isFinite(v)) return 0;
  if (v > 0) return 0;
  if (v < -30) return -30;
  return v;
}

export function syncAttUi(db) {
  const slider = document.getElementById("att-slider");
  const input = document.getElementById("att-input");
  const info = document.getElementById("att-gain-info");
  if (slider) slider.value = String(db);
  if (input) input.value = String(db);
  if (info) info.textContent = attGainStr(db);
}

export async function applyAttenuation(db, source) {
  const v = clampAttDb(db);
  currentSettings.tx_attenuation_db = v;
  syncAttUi(v);
  const status = document.getElementById("att-status");
  try {
    if (window.__TAURI__ && window.__TAURI__.core) {
      await invoke("save_settings", { settings: currentSettings });
    }
    if (status) {
      status.textContent = source
        ? t("att.applied", { source, db: v.toFixed(1), time: now() })
        : t("att.applied_no_source", { db: v.toFixed(1), time: now() });
    }
  } catch (err) {
    if (status) status.textContent = t("status.error_prefix", { err });
  }
}

export function median(values) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

export function mean(values) {
  if (values.length === 0) return null;
  return values.reduce((a, b) => a + b, 0) / values.length;
}

export function renderCascade() {
  const tbody = document.getElementById("cascade-tbody");
  const medEl = document.getElementById("cascade-median");
  const meanEl = document.getElementById("cascade-mean");
  const apply = document.getElementById("cascade-apply");
  if (!tbody) return;
  if (cascadeFeedback.length === 0) {
    tbody.innerHTML = `<tr><td colspan="3" class="cascade-empty">Aucun rapport.</td></tr>`;
    if (medEl) medEl.textContent = "—";
    if (meanEl) meanEl.textContent = "—";
    if (apply) apply.disabled = true;
    return;
  }
  tbody.innerHTML = cascadeFeedback
    .map((row, i) =>
      `<tr><td>${escapeHtml(row.call)}</td><td>${row.db.toFixed(1)}</td>` +
      `<td><button class="cascade-row-del" data-idx="${i}" title="Supprimer">✕</button></td></tr>`
    )
    .join("");
  for (const btn of tbody.querySelectorAll(".cascade-row-del")) {
    btn.addEventListener("click", (ev) => {
      const idx = Number(ev.currentTarget.dataset.idx);
      if (Number.isFinite(idx)) {
        cascadeFeedback.splice(idx, 1);
        renderCascade();
      }
    });
  }
  const vals = cascadeFeedback.map(r => r.db);
  if (medEl) medEl.textContent = `${median(vals).toFixed(1)} dB`;
  if (meanEl) meanEl.textContent = `${mean(vals).toFixed(1)} dB`;
  if (apply) apply.disabled = false;
}

export function setupChannelTab() {
  const slider = document.getElementById("att-slider");
  const input = document.getElementById("att-input");
  const reset = document.getElementById("att-reset");
  const initialDb = clampAttDb(Number(currentSettings.tx_attenuation_db) || 0);
  syncAttUi(initialDb);
  if (slider) {
    slider.addEventListener("input", () => {
      const v = clampAttDb(Number(slider.value));
      syncAttUi(v);
    });
    slider.addEventListener("change", () => {
      applyAttenuation(Number(slider.value), "slider");
    });
  }
  if (input) {
    input.addEventListener("change", () => {
      applyAttenuation(Number(input.value), "saisie");
    });
  }
  if (reset) {
    reset.addEventListener("click", () => applyAttenuation(0, "reset"));
  }
  const callInput = document.getElementById("cascade-call");
  const dbInput = document.getElementById("cascade-db");
  const addBtn = document.getElementById("cascade-add");
  const applyBtn = document.getElementById("cascade-apply");
  const clearBtn = document.getElementById("cascade-clear");
  function addCascadeEntry() {
    const call = (callInput && callInput.value || "").trim().toUpperCase();
    const db = Number(dbInput && dbInput.value);
    if (!Number.isFinite(db)) return;
    cascadeFeedback.push({ call: call || "?", db });
    if (callInput) callInput.value = "";
    if (dbInput) dbInput.value = "";
    if (callInput) callInput.focus();
    renderCascade();
  }
  if (addBtn) addBtn.addEventListener("click", addCascadeEntry);
  for (const el of [callInput, dbInput]) {
    if (!el) continue;
    el.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        ev.preventDefault();
        addCascadeEntry();
      }
    });
  }
  if (applyBtn) {
    applyBtn.addEventListener("click", () => {
      const vals = cascadeFeedback.map(r => r.db);
      const m = median(vals);
      if (m !== null) applyAttenuation(m, t("channel.cascade_median_source"));
    });
  }
  if (clearBtn) {
    clearBtn.addEventListener("click", () => {
      cascadeFeedback = [];
      renderCascade();
    });
  }
  renderCascade();
}
