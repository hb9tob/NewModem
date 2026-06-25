// Shared layer — pure formatters & MIME constants.
// Zero DOM, zero app state, imports nothing. Leaf of the module graph
// (mirrors modem-framing in the Rust workspace: no DSP, no IO).

// Mapping aligned with modem-core/src/app_header.rs :: mime
//   0 = BINARY, 1 = TEXT, 2 = IMAGE_AVIF, 3 = IMAGE_JPEG, 4 = IMAGE_PNG,
//   5 = ZSTD (non-image file decompressed RX-side by the Rust worker).
export const MIME_TYPES = {
  0: "application/octet-stream",
  1: "text/plain",
  2: "image/avif",
  3: "image/jpeg",
  4: "image/png",
  5: "application/zstd",
};

export const MIME_BINARY = 0;

export const MIME_TEXT = 1;

export const MIME_IMAGE_AVIF = 2;

export const MIME_IMAGE_JPEG = 3;

export const MIME_IMAGE_PNG = 4;

export const MIME_ZSTD = 5;

export function mimeToExt(code) {
  return MIME_TYPES[code] || "application/octet-stream";
}

export function isImageMime(code) {
  return [MIME_IMAGE_AVIF, MIME_IMAGE_JPEG, MIME_IMAGE_PNG].includes(code);
}

export function now() {
  return new Date().toLocaleTimeString();
}

export function escapeHtml(s) {
  if (s == null) return "";
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export function numOr(v, fallback) {
  const n = parseFloat(v);
  return Number.isFinite(n) ? n : fallback;
}

export function fmtSeconds(s) {
  if (!Number.isFinite(s)) return "—";
  const m = Math.floor(s / 60);
  const r = Math.round(s - m * 60);
  return `${m}:${String(r).padStart(2, "0")}`;
}

export function txFormatBytes(n) {
  if (n == null) return "—";
  if (n < 1024) return `${n} o`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} Kio`;
  return `${(n / 1024 / 1024).toFixed(2)} Mio`;
}

export const IMAGE_EXTS = new Set(["png", "jpg", "jpeg", "avif", "webp", "gif", "bmp"]);

export function isImageFilename(name) {
  const lower = (name || "").toLowerCase();
  const dot = lower.lastIndexOf(".");
  if (dot < 0) return false;
  return IMAGE_EXTS.has(lower.slice(dot + 1));
}

export function formatTimestamp(unixSeconds) {
  if (!unixSeconds) return "—";
  const d = new Date(unixSeconds * 1000);
  return d.toLocaleString("fr-CH", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function formatBytes(n) {
  if (!n || n < 1024) return `${n || 0} o`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} Kio`;
  return `${(n / (1024 * 1024)).toFixed(2)} Mio`;
}

export function fmtNumOrDash(v, digits) {
  if (v === null || v === undefined) return "—";
  const n = Number(v);
  if (!Number.isFinite(n)) return "—";
  return n.toFixed(digits ?? 2);
}
