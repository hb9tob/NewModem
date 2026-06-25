// Shared widget layer — kiosk touch input: the full-screen <select> picker and
// the on-screen virtual keyboard (frequency inputs get a numeric pad with
// favorites + step rows). Global capture-phase listeners; reaches the SDR
// frequency MRU helpers but no tab.
import { backendIdForFreqInput, freqFavoritesArray, isFreqInputId, pushFreqMru } from "./sdr.js";

const VIRT_KB_QWERTY_ROWS = [
  ["q","w","e","r","t","y","u","i","o","p"],
  ["a","s","d","f","g","h","j","k","l"],
  ["z","x","c","v","b","n","m"],
];
const VIRT_KB_SYMBOLS_ROWS = [
  ["1","2","3","4","5","6","7","8","9","0"],
  ["@","#","_","-",".",",","/",":","="],
  ["+","*","(",")","'","\"","?","!","%"],
];
const VIRT_KB_NUMERIC_ROWS = [
  ["1","2","3"],
  ["4","5","6"],
  ["7","8","9"],
  ["-",".","0"],
];

export const selectPicker = {
  modal: null,
  labelEl: null,
  listEl: null,
  closeEl: null,
  target: null,
};

export function setupSelectPicker() {
  selectPicker.modal = document.getElementById("select-picker-modal");
  selectPicker.labelEl = document.getElementById("select-picker-label");
  selectPicker.listEl = document.getElementById("select-picker-list");
  selectPicker.closeEl = document.getElementById("select-picker-cancel");
  if (!selectPicker.modal) return;

  // Capture phase — must run before WebKitGTK opens its native popup.
  // Mousedown is the right hook: click is fired AFTER the native
  // popup has already opened (and consumed the gesture on touch).
  document.addEventListener("mousedown", (e) => {
    if (!document.body.classList.contains("kiosk-mode")) return;
    let el = e.target;
    if (el instanceof HTMLOptionElement) el = el.parentElement;
    if (!(el instanceof HTMLSelectElement)) return;
    if (el.disabled) return;
    if (el.dataset.selectPickerSkip === "1") return;
    if (selectPicker.modal.contains(el)) return;
    e.preventDefault();
    e.stopPropagation();
    el.blur();
    openSelectPicker(el);
  }, /*capture=*/true);

  // Same hook on `keydown` Space/Enter, in case the select is
  // reached by Tab (the kiosk has no keyboard, but the desktop devs
  // can still keyboard-navigate).
  document.addEventListener("keydown", (e) => {
    if (!document.body.classList.contains("kiosk-mode")) return;
    if (e.key !== " " && e.key !== "Enter" && e.key !== "ArrowDown") return;
    const el = e.target;
    if (!(el instanceof HTMLSelectElement)) return;
    if (el.disabled || el.dataset.selectPickerSkip === "1") return;
    e.preventDefault();
    openSelectPicker(el);
  }, /*capture=*/true);

  selectPicker.closeEl.addEventListener("click", closeSelectPicker);
  selectPicker.modal.addEventListener("click", (e) => {
    if (e.target === selectPicker.modal) closeSelectPicker();
  });
  document.addEventListener("keydown", (e) => {
    if (selectPicker.modal.hidden) return;
    if (e.key === "Escape") {
      e.preventDefault();
      closeSelectPicker();
    }
  });
}

export function selectLabel(sel) {
  // Surrounding <label>'s text minus the <select>'s current option text,
  // falls back to <legend>, then to the id. Matches the virtKb pattern.
  const lab = sel.closest("label");
  if (lab) {
    const txt = (lab.textContent || "")
      .replace(sel.options[sel.selectedIndex]?.textContent || "", "")
      .trim();
    if (txt) return txt.replace(/\s+/g, " ").slice(0, 80);
  }
  const fs = sel.closest("fieldset");
  const lg = fs && fs.querySelector("legend");
  if (lg && lg.textContent) return lg.textContent.trim().slice(0, 80);
  return sel.id || "Choisir";
}

export function openSelectPicker(sel) {
  selectPicker.target = sel;
  selectPicker.labelEl.textContent = selectLabel(sel);
  const list = selectPicker.listEl;
  list.innerHTML = "";
  for (const opt of sel.options) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "select-picker-row";
    if (opt.classList.contains("experimental-option")) {
      row.classList.add("select-picker-experimental");
    }
    if (opt.value === sel.value) {
      row.classList.add("select-picker-current");
    }
    if (opt.disabled) row.classList.add("select-picker-disabled");
    row.dataset.value = opt.value;
    row.textContent = opt.textContent;
    if (opt.disabled) row.disabled = true;
    row.addEventListener("click", () => {
      const target = selectPicker.target;
      if (target && target.value !== opt.value) {
        target.value = opt.value;
        // Notify the rest of the app exactly as the native popup does.
        target.dispatchEvent(new Event("input", { bubbles: true }));
        target.dispatchEvent(new Event("change", { bubbles: true }));
      }
      closeSelectPicker();
    });
    list.appendChild(row);
  }
  selectPicker.modal.hidden = false;
  // Scroll the current selection into view so opening on a long list
  // (10 modem profiles, 39 CTCSS tones) doesn't always start from top.
  const cur = list.querySelector(".select-picker-current");
  if (cur) cur.scrollIntoView({ block: "center" });
}

export function closeSelectPicker() {
  if (!selectPicker.modal || selectPicker.modal.hidden) return;
  selectPicker.modal.hidden = true;
  selectPicker.target = null;
  selectPicker.listEl.innerHTML = "";
}

export const virtKb = {
  modal: null,
  rowsEl: null,
  displayEl: null,
  labelEl: null,
  okBtn: null,
  cancelBtn: null,
  closeBtn: null,
  // Live state — reset on every open():
  target: null,        // the original <input> element
  draft: "",           // editable string buffer
  layout: "alpha",     // "alpha" | "symbols" | "numeric"
  shift: false,        // true = uppercase letters in alpha mode
  capsLock: false,     // sticky shift (callsign field auto-engages)
  // Frequency-keypad step (kHz), persisted across keypad close/re-open.
  stepKHz: 6.25,
};

export function setupVirtKeyboard() {
  virtKb.modal = document.getElementById("virt-keyboard-modal");
  virtKb.rowsEl = document.getElementById("virt-kb-rows");
  virtKb.displayEl = document.getElementById("virt-kb-value");
  virtKb.labelEl = document.getElementById("virt-kb-label");
  virtKb.okBtn = document.getElementById("virt-kb-ok");
  virtKb.cancelBtn = document.getElementById("virt-kb-cancel-btn");
  virtKb.closeBtn = document.getElementById("virt-kb-cancel");
  if (!virtKb.modal) return;

  // Single delegated focusin handler (cheap, no re-attach when DOM
  // changes). Filters by input.type so checkboxes / radios / file pickers
  // don't trigger the keyboard. Hidden inputs in modals (e.g. file
  // picker) are skipped via `:not([hidden])`.
  document.addEventListener("focusin", (e) => {
    if (!document.body.classList.contains("kiosk-mode")) return;
    const t = e.target;
    if (!(t instanceof HTMLInputElement)) return;
    if (t.dataset.virtKbSkip === "1") return;
    const type = (t.type || "text").toLowerCase();
    if (type !== "text" && type !== "number" && type !== "search" && type !== "tel" && type !== "url") return;
    // Already inside the keyboard? avoid recursion (the display is a
    // <span>, not an input, but defensive).
    if (virtKb.modal.contains(t)) return;
    // Unfocus the input so the OS soft-keyboard (if any) doesn't try to
    // appear underneath. We keep a reference for the commit.
    t.blur();
    openVirtKeyboard(t);
  });

  // OK / Cancel / outside-tap / Esc.
  virtKb.okBtn.addEventListener("click", () => closeVirtKeyboard(true));
  virtKb.cancelBtn.addEventListener("click", () => closeVirtKeyboard(false));
  virtKb.closeBtn.addEventListener("click", () => closeVirtKeyboard(false));
  virtKb.modal.addEventListener("click", (e) => {
    if (e.target === virtKb.modal) closeVirtKeyboard(false);
  });
  document.addEventListener("keydown", (e) => {
    if (virtKb.modal.hidden) return;
    if (e.key === "Escape") {
      e.preventDefault();
      closeVirtKeyboard(false);
    } else if (e.key === "Enter") {
      e.preventDefault();
      closeVirtKeyboard(true);
    }
  });
}

export function openVirtKeyboard(input) {
  virtKb.target = input;
  virtKb.draft = input.value || "";
  // Pick layout from input.type and id.
  const type = (input.type || "text").toLowerCase();
  if (type === "number") {
    virtKb.layout = "numeric";
    virtKb.shift = false;
    virtKb.capsLock = false;
  } else {
    virtKb.layout = "alpha";
    // Callsign field — uppercase locked, ASCII alphanum only. Same
    // pragmatic behaviour as a real radio's callsign editor.
    virtKb.capsLock = (input.id === "callsign-input");
    virtKb.shift = virtKb.capsLock;
  }
  virtKb.labelEl.textContent = inputLabel(input);
  virtKb.modal.hidden = false;
  renderVirtKeyboardLayout();
  refreshVirtKbDisplay();
}

export function closeVirtKeyboard(commit) {
  if (!virtKb.modal || virtKb.modal.hidden) return;
  const target = virtKb.target;
  if (commit && target) {
    const max = parseInt(target.getAttribute("maxlength") || "0", 10);
    let out = virtKb.draft;
    if (max > 0) out = out.slice(0, max);
    target.value = out;
    // Notify any change listener wired by the rest of the app
    // (persistSettings, refreshTxEstimate, …).
    target.dispatchEvent(new Event("input", { bubbles: true }));
    target.dispatchEvent(new Event("change", { bubbles: true }));
    // Frequency inputs : add the validated value to the MRU
    // favorites so the next keypad open offers it as a quick-pick.
    if (isFreqInputId(target.id)) {
      const mhz = parseFloat(out);
      if (Number.isFinite(mhz) && mhz > 0) {
        // fire-and-forget — own save flush, doesn't block close.
        pushFreqMru(mhz, target);
      }
    }
  }
  virtKb.modal.hidden = true;
  virtKb.target = null;
  virtKb.draft = "";
}

export function inputLabel(input) {
  // Prefer the surrounding <label>'s direct text node, fall back to
  // placeholder, then to the id.
  const lab = input.closest("label");
  if (lab) {
    const txt = (lab.textContent || "").replace(input.value || "", "").trim();
    if (txt) return txt.replace(/\s+/g, " ").slice(0, 60);
  }
  if (input.placeholder) return input.placeholder;
  return input.id || "Saisie";
}

export function renderVirtKeyboardLayout() {
  const rows = virtKb.rowsEl;
  rows.innerHTML = "";
  if (virtKb.layout === "numeric") {
    // Extra rows for frequency inputs: MRU favorites + step buttons.
    // Detected by input id; falls back to a plain numeric pad for any
    // other `<input type="number">` (gain dB, attenuation, etc).
    if (isFreqInputId(virtKb.target?.id)) {
      renderVirtKbFavoritesRow();
      renderVirtKbStepRow();
    }
    for (const row of VIRT_KB_NUMERIC_ROWS) {
      const r = document.createElement("div");
      r.className = "virt-kb-row";
      for (const k of row) r.appendChild(makeKbKey(k));
      rows.appendChild(r);
    }
    const trailer = document.createElement("div");
    trailer.className = "virt-kb-row";
    trailer.appendChild(makeKbKey("⌫", "back", "wide-2 special"));
    rows.appendChild(trailer);
    return;
  }
  const rowsData = virtKb.layout === "symbols" ? VIRT_KB_SYMBOLS_ROWS : VIRT_KB_QWERTY_ROWS;
  for (const row of rowsData) {
    const r = document.createElement("div");
    r.className = "virt-kb-row";
    for (const k of row) {
      const display = (virtKb.layout === "alpha" && (virtKb.shift || virtKb.capsLock))
        ? k.toUpperCase()
        : k;
      r.appendChild(makeKbKey(display, k));
    }
    rows.appendChild(r);
  }
  // Last row : layout-specific specials.
  const last = document.createElement("div");
  last.className = "virt-kb-row";
  if (virtKb.layout === "alpha") {
    const shiftLabel = virtKb.capsLock ? "⇪" : "⇧";
    last.appendChild(makeKbKey(shiftLabel, "shift", "wide-2 special"));
    last.appendChild(makeKbKey("?123", "to-symbols", "wide-2 special"));
    last.appendChild(makeKbKey("Esp.", "space", "wide-3 special"));
    last.appendChild(makeKbKey("⌫", "back", "wide-2 special"));
  } else {
    last.appendChild(makeKbKey("ABC", "to-alpha", "wide-2 special"));
    last.appendChild(makeKbKey("Esp.", "space", "wide-3 special"));
    last.appendChild(makeKbKey("⌫", "back", "wide-2 special"));
  }
  rows.appendChild(last);
}

export function makeKbKey(label, action, extraClass) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "virt-kb-key" + (extraClass ? " " + extraClass : "");
  b.textContent = label;
  b.dataset.action = action || label;
  b.addEventListener("click", () => onVirtKbKey(b.dataset.action));
  return b;
}

export function onVirtKbKey(action) {
  switch (action) {
    case "back":
      virtKb.draft = virtKb.draft.slice(0, -1);
      break;
    case "space":
      appendVirtKbChar(" ");
      break;
    case "shift":
      if (virtKb.shift && !virtKb.capsLock) {
        // 1st tap : shift on. 2nd consecutive tap : caps-lock.
        virtKb.capsLock = true;
      } else if (virtKb.capsLock) {
        // Tap while capsLock : turn everything off.
        virtKb.capsLock = false;
        virtKb.shift = false;
      } else {
        virtKb.shift = true;
      }
      renderVirtKeyboardLayout();
      return;
    case "to-symbols":
      virtKb.layout = "symbols";
      renderVirtKeyboardLayout();
      return;
    case "to-alpha":
      virtKb.layout = "alpha";
      renderVirtKeyboardLayout();
      return;
    default:
      // Single-character key: respect shift / capsLock for letters.
      let c = action;
      if (virtKb.layout === "alpha" && (virtKb.shift || virtKb.capsLock) && c.length === 1) {
        c = c.toUpperCase();
      }
      appendVirtKbChar(c);
      // Auto-release a one-shot shift (capsLock keeps it on).
      if (virtKb.shift && !virtKb.capsLock) {
        virtKb.shift = false;
        renderVirtKeyboardLayout();
      }
      break;
  }
  refreshVirtKbDisplay();
}

export function appendVirtKbChar(c) {
  if (virtKb.target) {
    const max = parseInt(virtKb.target.getAttribute("maxlength") || "0", 10);
    if (max > 0 && virtKb.draft.length >= max) return;
  }
  virtKb.draft += c;
}

export function refreshVirtKbDisplay() {
  // Replace ` ` to keep an empty draft visible (otherwise the
  // height collapses).
  virtKb.displayEl.textContent = virtKb.draft.length ? virtKb.draft : " ";
}

export const STEP_OPTIONS_KHZ = [5.0, 6.25, 12.5, 25.0];

export function renderVirtKbFavoritesRow() {
  const target = virtKb.target;
  const backendId = backendIdForFreqInput(target);
  const favs = backendId ? freqFavoritesArray(backendId) : [];
  if (favs.length === 0) return;
  const row = document.createElement("div");
  row.className = "virt-kb-row virt-kb-favs";
  for (const hz of favs) {
    const mhz = (hz / 1e6).toFixed(3);
    const b = document.createElement("button");
    b.type = "button";
    b.className = "virt-kb-key special";
    b.textContent = mhz;
    b.title = `Charger ${mhz} MHz`;
    b.addEventListener("click", () => {
      // Strip trailing zeros so "145.500" displays compact, but
      // keep the decimal point so the user knows it's fractional.
      virtKb.draft = mhz.replace(/0+$/, "").replace(/\.$/, "");
      refreshVirtKbDisplay();
    });
    row.appendChild(b);
  }
  virtKb.rowsEl.appendChild(row);
}

export function renderVirtKbStepRow() {
  const row = document.createElement("div");
  row.className = "virt-kb-row virt-kb-step-row";
  // Step selector — taps cycle through STEP_OPTIONS_KHZ.
  const stepBtn = document.createElement("button");
  stepBtn.type = "button";
  stepBtn.className = "virt-kb-key special wide-2";
  stepBtn.textContent = `Pas: ${virtKb.stepKHz} kHz`;
  stepBtn.title = "Cycle 5 / 6.25 / 12.5 / 25 kHz";
  stepBtn.addEventListener("click", () => {
    const idx = STEP_OPTIONS_KHZ.indexOf(virtKb.stepKHz);
    virtKb.stepKHz = STEP_OPTIONS_KHZ[(idx + 1) % STEP_OPTIONS_KHZ.length];
    stepBtn.textContent = `Pas: ${virtKb.stepKHz} kHz`;
  });
  row.appendChild(stepBtn);

  const minusBtn = document.createElement("button");
  minusBtn.type = "button";
  minusBtn.className = "virt-kb-key";
  minusBtn.textContent = "−";
  minusBtn.addEventListener("click", () => stepDraft(-1));
  row.appendChild(minusBtn);

  const plusBtn = document.createElement("button");
  plusBtn.type = "button";
  plusBtn.className = "virt-kb-key";
  plusBtn.textContent = "+";
  plusBtn.addEventListener("click", () => stepDraft(+1));
  row.appendChild(plusBtn);

  virtKb.rowsEl.appendChild(row);
}

export function stepDraft(direction) {
  // Parse the current draft as MHz (accept partials like "145." or
  // ""). Empty / unparseable → start from 0.
  const cur = parseFloat(virtKb.draft);
  const start = Number.isFinite(cur) ? cur : 0;
  const deltaMHz = (virtKb.stepKHz / 1000.0) * direction;
  const next = start + deltaMHz;
  // 5 decimals = 10 Hz precision, more than enough for amateur
  // channel rasters. Trim trailing zeros for compact display.
  let fixed = next.toFixed(5).replace(/0+$/, "").replace(/\.$/, "");
  if (fixed === "" || fixed === "-") fixed = "0";
  virtKb.draft = fixed;
  refreshVirtKbDisplay();
}
