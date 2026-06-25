// Shared layer — generic DOM helpers. No app state, no Tauri, imports
// nothing. Leaf of the module graph.

export function getSelectedBackendId(selectId) {
  const sel = document.getElementById(selectId);
  if (!sel) return null;
  const opt = sel.options[sel.selectedIndex];
  if (!opt || !opt.dataset) return null;
  const b = opt.dataset.backend;
  return (b && b !== "audio") ? b : null;
}

export function makeRow() {
  const r = document.createElement("div");
  r.className = "pluto-row";
  return r;
}

export function makeFieldLabel(text) {
  const s = document.createElement("span");
  s.className = "pluto-field-label";
  s.textContent = text;
  return s;
}

export function rxIsRunning() {
  const stopBtn = document.getElementById("btn-stop");
  return !!(stopBtn && !stopBtn.disabled);
}
