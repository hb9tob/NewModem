// Overlays tab — the 5 logo/callsign overlay slots + the editor. Owns the
// overlay slots in currentSettings; emits settings:persist and tx:recompress
// (re-burn the TX image with the new overlay) so it never imports settings/tx.
import { invoke, convertFileSrc } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "../lib/state.js";
import { logEvent } from "../lib/log.js";
import { numOr } from "../lib/format.js";
import { emit } from "../lib/bus.js";

export function makeDefaultOverlays() {
  return [
    { name: "Aucun", text: null, logo: null },
    { name: "Overlay 1", text: null, logo: null },
    { name: "Overlay 2", text: null, logo: null },
    { name: "Overlay 3", text: null, logo: null },
    { name: "Overlay 4", text: null, logo: null },
  ];
}

export function ensureOverlaySlots() {
  if (!Array.isArray(currentSettings.overlays) || currentSettings.overlays.length === 0) {
    currentSettings.overlays = makeDefaultOverlays();
  }
  while (currentSettings.overlays.length < 5) {
    const i = currentSettings.overlays.length;
    currentSettings.overlays.push({ name: i === 0 ? "Aucun" : `Overlay ${i}`, text: null, logo: null });
  }
  if (typeof currentSettings.active_overlay !== "number"
      || currentSettings.active_overlay < 0
      || currentSettings.active_overlay >= currentSettings.overlays.length) {
    currentSettings.active_overlay = 0;
  }
}

export function getActiveOverlayPayload() {
  ensureOverlaySlots();
  const idx = currentSettings.active_overlay | 0;
  if (idx <= 0 || idx >= currentSettings.overlays.length) return null;
  const slot = currentSettings.overlays[idx];
  if (!slot) return null;
  const text = slot.text && slot.text.content ? slot.text : null;
  const logo = slot.logo && slot.logo.filename ? slot.logo : null;
  if (!text && !logo) return null;
  return { name: slot.name || "", text, logo };
}

export function setSlotLabel(idx, name) {
  const span = document.querySelector(`.ov-slot-label[data-slot="${idx}"]`);
  if (!span) return;
  const empty = !name || /^Overlay \d+$/.test(name);
  span.textContent = name && name.trim() ? name : `Overlay ${idx}`;
  span.classList.toggle("ov-empty", empty);
}

export function refreshSlotLabels() {
  for (let i = 1; i <= 4; i++) {
    const slot = currentSettings.overlays[i] || {};
    setSlotLabel(i, slot.name || "");
  }
}

export function getEditingSlot() {
  const idx = currentSettings.active_overlay | 0;
  if (idx <= 0) return null;
  return currentSettings.overlays[idx] || null;
}

export function applyOverlayEditorFromState() {
  const editor = document.getElementById("ov-editor");
  const slot = getEditingSlot();
  if (!editor) return;
  if (!slot) {
    editor.hidden = true;
    return;
  }
  editor.hidden = false;
  document.getElementById("ov-name").value = slot.name || "";
  // Text element.
  const t = slot.text || {};
  document.getElementById("ov-text-enable").checked = !!slot.text;
  document.getElementById("ov-text-content").value = t.content || "";
  document.getElementById("ov-text-anchor").value = t.anchor || "bottom_right";
  document.getElementById("ov-text-mx").value = (t.margin_x_pct ?? 2);
  document.getElementById("ov-text-my").value = (t.margin_y_pct ?? 2);
  document.getElementById("ov-text-h").value = (t.height_pct ?? 6);
  document.getElementById("ov-text-color").value = t.color || "#ffffff";
  document.getElementById("ov-text-halo").checked = t.halo !== false;
  // Logo element.
  const l = slot.logo || {};
  document.getElementById("ov-logo-enable").checked = !!slot.logo;
  document.getElementById("ov-logo-anchor").value = l.anchor || "top_left";
  document.getElementById("ov-logo-mx").value = (l.margin_x_pct ?? 2);
  document.getElementById("ov-logo-my").value = (l.margin_y_pct ?? 2);
  document.getElementById("ov-logo-size").value = (l.size_pct ?? 12);
  refreshLogoPreview(l.filename || "");
}

export async function refreshLogoPreview(filename) {
  const nameEl = document.getElementById("ov-logo-name");
  const imgEl = document.getElementById("ov-logo-preview");
  if (!nameEl || !imgEl) return;
  if (!filename) {
    nameEl.textContent = "—";
    imgEl.hidden = true;
    imgEl.src = "";
    return;
  }
  nameEl.textContent = filename;
  if (window.__TAURI__ && window.__TAURI__.core) {
    try {
      const dir = await invoke("overlays_logos_dir");
      const sep = dir.includes("\\") ? "\\" : "/";
      const url = `${convertFileSrc(`${dir}${sep}${filename}`)}?v=${Date.now()}`;
      imgEl.src = url;
      imgEl.hidden = false;
    } catch (err) {
      console.error("overlays_logos_dir", err);
      imgEl.hidden = true;
    }
  }
}

export function readOverlayEditorIntoState() {
  const slot = getEditingSlot();
  if (!slot) return;
  slot.name = (document.getElementById("ov-name").value || "").trim();
  // Text.
  const tEnabled = document.getElementById("ov-text-enable").checked;
  if (tEnabled) {
    slot.text = {
      content: document.getElementById("ov-text-content").value || "",
      anchor: document.getElementById("ov-text-anchor").value,
      margin_x_pct: numOr(document.getElementById("ov-text-mx").value, 2),
      margin_y_pct: numOr(document.getElementById("ov-text-my").value, 2),
      height_pct: numOr(document.getElementById("ov-text-h").value, 6),
      color: document.getElementById("ov-text-color").value || "#ffffff",
      halo: document.getElementById("ov-text-halo").checked,
    };
  } else {
    slot.text = null;
  }
  // Logo. The filename is owned by the import flow, so we preserve it
  // even when the user toggles "Logo" off + on; we only nullify on off.
  const lEnabled = document.getElementById("ov-logo-enable").checked;
  if (lEnabled) {
    slot.logo = {
      filename: (slot.logo && slot.logo.filename) || "",
      anchor: document.getElementById("ov-logo-anchor").value,
      margin_x_pct: numOr(document.getElementById("ov-logo-mx").value, 2),
      margin_y_pct: numOr(document.getElementById("ov-logo-my").value, 2),
      size_pct: numOr(document.getElementById("ov-logo-size").value, 12),
    };
  } else {
    slot.logo = null;
  }
}

export let _overlayCommitTimer = null;

export function commitOverlayChange() {
  readOverlayEditorIntoState();
  refreshSlotLabels();
  if (_overlayCommitTimer) clearTimeout(_overlayCommitTimer);
  _overlayCommitTimer = setTimeout(() => {
    _overlayCommitTimer = null;
    emit("settings:persist");
    emit("tx:recompress");
  }, 250);
}

export async function pickOverlayLogo(file) {
  if (!file) return;
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  const buf = await file.arrayBuffer();
  try {
    const filename = await invoke("overlays_import_logo", {
      bytes: Array.from(new Uint8Array(buf)),
      originalName: file.name || "logo.png",
    });
    const slot = getEditingSlot();
    if (!slot) return;
    if (!slot.logo) {
      slot.logo = { filename, anchor: "top_left", margin_x_pct: 2, margin_y_pct: 2, size_pct: 12 };
    } else {
      slot.logo.filename = filename;
    }
    document.getElementById("ov-logo-enable").checked = true;
    applyOverlayEditorFromState();
    commitOverlayChange();
  } catch (err) {
    console.error("overlays_import_logo", err);
    alert(t("status.error_import_logo", { err }));
  }
}

export function setupOverlaysTab() {
  ensureOverlaySlots();
  // Slot selection (radio bar).
  const radios = document.querySelectorAll('input[name="active-overlay"]');
  radios.forEach(r => {
    r.addEventListener("change", () => {
      currentSettings.active_overlay = parseInt(r.value, 10) | 0;
      applyOverlayEditorFromState();
      emit("settings:persist");
      emit("tx:recompress");
    });
  });
  // All editable inputs flow through commitOverlayChange.
  const inputIds = [
    "ov-name",
    "ov-text-enable", "ov-text-content", "ov-text-anchor",
    "ov-text-mx", "ov-text-my", "ov-text-h", "ov-text-color", "ov-text-halo",
    "ov-logo-enable", "ov-logo-anchor",
    "ov-logo-mx", "ov-logo-my", "ov-logo-size",
  ];
  for (const id of inputIds) {
    const el = document.getElementById(id);
    if (!el) continue;
    el.addEventListener("input", commitOverlayChange);
    el.addEventListener("change", commitOverlayChange);
  }
  // Logo file picker.
  const pick = document.getElementById("ov-logo-pick");
  const fileInput = document.getElementById("ov-logo-input");
  if (pick && fileInput) {
    pick.addEventListener("click", () => fileInput.click());
    fileInput.addEventListener("change", () => {
      const f = fileInput.files && fileInput.files[0];
      if (f) pickOverlayLogo(f);
      fileInput.value = "";
    });
  }
}

export function applyOverlaysToUI() {
  ensureOverlaySlots();
  refreshSlotLabels();
  const idx = currentSettings.active_overlay | 0;
  const radio = document.querySelector(`input[name="active-overlay"][value="${idx}"]`);
  if (radio) radio.checked = true;
  applyOverlayEditorFromState();
}
