// Sessions tab — the on-disk RaptorQ session registry table.
// Self-contained: the live session_armed/progress/decoded events (wired in
// main.js) call upsertSession; refreshSessions reloads from disk.
import { invoke } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { escapeHtml } from "../lib/format.js";
import { logEvent } from "../lib/log.js";

export const sessionRegistry = new Map();

export async function refreshSessions() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    const list = await invoke("list_sessions");
    sessionRegistry.clear();
    for (const meta of list) {
      // `received_esis` is recomputed from the on-disk blob by the backend,
      // so a partially-received session shows its real progress (e.g.
      // 247/250) instead of collapsing to 0 %. Live session_progress events
      // still refine it during an active reception. Fall back to the old
      // decoded ? k : 0 heuristic for metas without the field (legacy).
      const received = Number.isFinite(meta.received_esis)
        ? meta.received_esis
        : (meta.decoded ? meta.k_symbols : 0);
      sessionRegistry.set(meta.session_id, {
        ...meta,
        received,
        cap_reached: false,
      });
    }
    renderSessionsTable();
  } catch (err) {
    logEvent("sessions_refresh_error", { message: String(err) });
  }
}

export function upsertSession(partial) {
  const id = partial.session_id;
  const prev = sessionRegistry.get(id) || {};
  sessionRegistry.set(id, { ...prev, ...partial });
  renderSessionsTable();
}

export function renderSessionsTable() {
  const tbody = document.getElementById("sessions-tbody");
  const countEl = document.getElementById("sessions-count");
  if (!tbody) return;
  const entries = Array.from(sessionRegistry.values()).sort(
    (a, b) => (b.created_at || 0) - (a.created_at || 0)
  );
  countEl.textContent =
    entries.length === 0
      ? t("sessions.count_zero")
      : entries.length === 1
        ? t("sessions.count_one")
        : t("sessions.count_many", { n: entries.length });
  if (entries.length === 0) {
    tbody.innerHTML = `<tr><td colspan="8" class="sessions-empty">${escapeHtml(t("sessions.empty"))}</td></tr>`;
    return;
  }
  tbody.innerHTML = entries.map(renderSessionRow).join("");
  // Wire delete buttons (fresh nodes each render).
  for (const btn of tbody.querySelectorAll(".btn-session-delete")) {
    btn.addEventListener("click", async (ev) => {
      const id = parseInt(ev.currentTarget.dataset.sid, 10);
      if (!Number.isFinite(id)) return;
      if (!confirm(t("sessions.delete_confirm", { id: id.toString(16).padStart(8, "0") }))) {
        return;
      }
      try {
        await invoke("delete_session", { sessionId: id });
        sessionRegistry.delete(id);
        renderSessionsTable();
      } catch (err) {
        logEvent("delete_session_error", { message: String(err) });
      }
    });
  }
}

export function renderSessionRow(s) {
  const idHex = s.session_id.toString(16).padStart(8, "0");
  const k = s.k_symbols || 0;
  const received = s.received || 0;
  const pct = k > 0 ? Math.min(100, Math.round((received * 100) / k)) : 0;
  const ratio = k > 0 ? received / k : 0;
  let fillClass = "";
  let statusClass = "waiting";
  let statusText = "attente";
  if (s.decoded) {
    fillClass = " done";
    statusClass = "done";
    statusText = t("sessions.status_decoded");
  } else if (s.cap_reached) {
    fillClass = " cap-reached";
    statusClass = "cap-reached";
    statusText = t("sessions.status_cap_reached");
  } else if (ratio >= 2.0) {
    fillClass = " cap-warn";
    statusClass = "cap-warn";
    statusText = t("sessions.status_degraded");
  }
  const filename = s.filename || "—";
  const callsign = s.callsign || "—";
  const profile = s.profile || "—";
  const widthPct = Math.min(100, (received * 100) / Math.max(k, 1));
  return `
    <tr>
      <td class="session-id">${idHex}</td>
      <td>${escapeHtml(callsign)}</td>
      <td>${escapeHtml(filename)}</td>
      <td>${escapeHtml(profile)}</td>
      <td>${received} / ${k}</td>
      <td class="progress-cell">
        <span class="progress-bar-bg"><span class="progress-bar-fill${fillClass}" style="width:${widthPct}%"></span></span>
        <span style="margin-left:8px;color:#888">${pct}%</span>
      </td>
      <td><span class="status-chip ${statusClass}">${statusText}</span></td>
      <td><button class="btn-session-delete" data-sid="${s.session_id}" title="${escapeHtml(t("sessions.delete_tip"))}">✕</button></td>
    </tr>`;
}
