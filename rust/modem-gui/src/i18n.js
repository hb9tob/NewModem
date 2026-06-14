// Lightweight i18n for the modem GUI.
//
// Translations are flat key/value JSON files (one per language) shipped
// next to this module. `t(key, params)` interpolates {placeholder}
// tokens. Static markup uses data-* attributes that `applyI18n(root)`
// scans:
//
//   <span data-i18n="rx.start">Démarrer</span>
//   <input data-i18n-placeholder="rx.notes" placeholder="…">
//   <button data-i18n-title="rx.raw_tip" title="…">
//   <span data-i18n-html="info.html_block">…</span>   (raw HTML, trust src)
//
// Switching language fires a `langchange` CustomEvent on `document` so
// dynamic JS code can re-render anything it has already injected.

const LS_KEY = "nbfm.gui.lang";
const FALLBACK = "fr";
const SUPPORTED = ["fr", "en"];

const dicts = {};   // lang -> { key: string }
let current = FALLBACK;
let loaded = false;

async function loadDict(lang) {
  if (dicts[lang]) return dicts[lang];
  const url = new URL(`./i18n/${lang}.json`, import.meta.url);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`i18n: cannot load ${lang} (${res.status})`);
  dicts[lang] = await res.json();
  return dicts[lang];
}

export async function initI18n() {
  let lang = null;
  try { lang = localStorage.getItem(LS_KEY); } catch (_) {}
  if (!SUPPORTED.includes(lang)) {
    // Fall back to the browser/UA language if recognised, else FR.
    const nav = (navigator.language || "fr").slice(0, 2).toLowerCase();
    lang = SUPPORTED.includes(nav) ? nav : FALLBACK;
  }
  current = lang;
  // Always load FR as the safety net for missing English keys.
  await loadDict(FALLBACK);
  if (current !== FALLBACK) await loadDict(current);
  loaded = true;
  document.documentElement.setAttribute("lang", current);
  applyI18n(document);
}

export function getLang() { return current; }
export function supportedLangs() { return SUPPORTED.slice(); }

export async function setLang(lang) {
  if (!SUPPORTED.includes(lang)) return;
  if (lang === current) return;
  await loadDict(lang);
  current = lang;
  try { localStorage.setItem(LS_KEY, lang); } catch (_) {}
  document.documentElement.setAttribute("lang", lang);
  applyI18n(document);
  document.dispatchEvent(new CustomEvent("langchange", { detail: { lang } }));
}

// Look up `key`; fall back to the FR dict, then to the key string
// itself so missing keys are visible in the UI rather than silent.
function lookup(key) {
  const d = dicts[current];
  if (d && Object.prototype.hasOwnProperty.call(d, key)) return d[key];
  const fb = dicts[FALLBACK];
  if (fb && Object.prototype.hasOwnProperty.call(fb, key)) return fb[key];
  return key;
}

function interpolate(str, params) {
  if (!params) return str;
  return str.replace(/\{(\w+)\}/g, (m, name) =>
    Object.prototype.hasOwnProperty.call(params, name) ? String(params[name]) : m);
}

export function t(key, params) {
  if (!loaded) return interpolate(lookup(key), params); // best-effort pre-init
  return interpolate(lookup(key), params);
}

// Walk `root` and apply translations. Safe to call multiple times.
export function applyI18n(root) {
  if (!root) root = document;
  // Text content.
  const textNodes = root.querySelectorAll("[data-i18n]");
  for (const el of textNodes) {
    const key = el.getAttribute("data-i18n");
    if (!key) continue;
    el.textContent = t(key);
  }
  // Raw HTML (only for keys whose value we control — used for
  // multi-line hints with <strong>, <code>, …).
  const htmlNodes = root.querySelectorAll("[data-i18n-html]");
  for (const el of htmlNodes) {
    const key = el.getAttribute("data-i18n-html");
    if (!key) continue;
    el.innerHTML = t(key);
  }
  // Attribute translations. Convention:
  //   data-i18n-attr-title  -> sets `title`
  //   data-i18n-attr-placeholder -> sets `placeholder`
  //   data-i18n-attr-aria-label -> sets `aria-label`
  // We also keep two short aliases for the two most common attrs.
  for (const el of root.querySelectorAll("[data-i18n-title]")) {
    el.setAttribute("title", t(el.getAttribute("data-i18n-title")));
  }
  for (const el of root.querySelectorAll("[data-i18n-placeholder]")) {
    el.setAttribute("placeholder", t(el.getAttribute("data-i18n-placeholder")));
  }
  for (const el of root.querySelectorAll("[data-i18n-aria-label]")) {
    el.setAttribute("aria-label", t(el.getAttribute("data-i18n-aria-label")));
  }
}
