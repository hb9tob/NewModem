// Shared layer — in-memory event log + the #event-log DOM list.
// The buffer is serialized to the Phase D collector at capture-submit time.
import { now } from "./format.js";

export const eventLogBuffer = [];

export function logEvent(name, data) {
  const tsMs = Date.now();
  eventLogBuffer.push({ ts_ms: tsMs, name, data: data ?? null });
  while (eventLogBuffer.length > 500) eventLogBuffer.shift();

  const log = document.getElementById("event-log");
  if (!log) return;
  const li = document.createElement("li");
  const t = document.createElement("span");
  t.className = "ev-time";
  t.textContent = now();
  const n = document.createElement("span");
  n.className = "ev-name";
  n.textContent = name;
  const body = document.createElement("span");
  body.textContent = data ? JSON.stringify(data) : "";
  li.appendChild(t);
  li.appendChild(n);
  li.appendChild(body);
  log.insertBefore(li, log.firstChild);
  while (log.children.length > 500) log.removeChild(log.lastChild);
}
