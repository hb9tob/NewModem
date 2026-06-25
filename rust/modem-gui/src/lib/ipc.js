// Shared layer — the single boundary that touches the global `window.__TAURI__`.
// Every other module imports these wrappers, so the Tauri surface is firewalled
// behind one file (mirrors modem-io: the one IO boundary in the Rust workspace).
//
// `withGlobalTauri: true` means the API is the global, NOT an `@tauri-apps/api`
// import (there is no bundler / node_modules served) — keep the global access
// HERE and nowhere else.

export function invoke(cmd, args) {
  if (!window.__TAURI__ || !window.__TAURI__.core) {
    return Promise.reject(new Error("Tauri unavailable"));
  }
  return window.__TAURI__.core.invoke(cmd, args);
}

export function listen(event, handler) {
  if (!window.__TAURI__ || !window.__TAURI__.event) {
    return Promise.resolve(() => {});
  }
  return window.__TAURI__.event.listen(event, handler);
}

export function convertFileSrc(path, protocol) {
  return window.__TAURI__?.core?.convertFileSrc(path, protocol);
}

export function getCurrentWindow() {
  return window.__TAURI__?.window?.getCurrentWindow();
}

// Best-effort external-URL open via the optional opener plugin; no-op (and
// swallowed) when the plugin isn't registered.
export async function openExternalUrl(url) {
  const op = window.__TAURI__?.opener;
  if (op && op.openUrl) return op.openUrl(url);
}
