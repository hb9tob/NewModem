// History tab — RX received-files + TX sent-sessions columns. The relay/resume
// buttons emit tx:relay / tx:resume with the path; the heavy TX-state restore
// (relayHistoryItem/resumeTxFromHistory) stays in main.js, so history imports no tx.
import { invoke, convertFileSrc } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { logEvent } from "../lib/log.js";
import { formatTimestamp, formatBytes } from "../lib/format.js";
import { openLightbox } from "../lib/lightbox.js";
import { emit } from "../lib/bus.js";

export function setupHistoryTab() {
  document
    .getElementById("btn-history-refresh")
    ?.addEventListener("click", refreshHistory);
}

export async function refreshHistory() {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    const [tx, rx] = await Promise.all([
      invoke("list_tx_history"),
      invoke("list_rx_history"),
    ]);
    renderHistoryColumn(tx, "tx");
    renderHistoryColumn(rx, "rx");
    const cnt = document.getElementById("history-count");
    if (cnt) cnt.textContent = `TX ${tx.length} · RX ${rx.length}`;
  } catch (err) {
    logEvent("history_error", { message: String(err) });
  }
}

export function renderHistoryColumn(items, kind) {
  const list = document.getElementById(`history-${kind}-list`);
  if (!list) return;
  list.innerHTML = "";
  if (!items.length) {
    const empty = document.createElement("div");
    empty.className = "history-empty";
    empty.textContent = t(kind === "tx" ? "history.tx_empty" : "history.rx_empty");
    list.appendChild(empty);
    return;
  }
  for (const item of items) {
    const card = document.createElement("div");
    card.className = "history-card";

    // Thumbnail (image or file icon).
    const thumb = document.createElement("div");
    thumb.className = "history-card-thumb";
    const previewPath = kind === "tx" ? item.file_path : item.preview_path;
    if (item.is_image) {
      const img = document.createElement("img");
      img.alt = item.filename;
      img.src = convertFileSrc(previewPath);
      img.addEventListener("dblclick", () =>
        openLightbox(convertFileSrc(previewPath), item.filename),
      );
      thumb.addEventListener("click", () =>
        openLightbox(convertFileSrc(previewPath), item.filename),
      );
      thumb.appendChild(img);
    } else {
      const icon = document.createElement("div");
      icon.className = "file-icon";
      icon.textContent = "📄";
      thumb.appendChild(icon);
      const fname = document.createElement("div");
      fname.className = "file-name";
      fname.textContent = item.filename;
      thumb.appendChild(fname);
      thumb.style.cursor = "default";
    }
    card.appendChild(thumb);

    // Bandeau metadata.
    const meta = document.createElement("div");
    meta.className = "history-card-meta";
    const row1 = document.createElement("div");
    row1.className = "row";
    const fname = document.createElement("span");
    fname.className = "filename";
    fname.title = item.filename;
    fname.textContent = item.filename;
    row1.appendChild(fname);
    const mode = document.createElement("span");
    mode.className = "mode";
    mode.textContent = item.mode;
    row1.appendChild(mode);
    meta.appendChild(row1);
    const row2 = document.createElement("div");
    row2.className = "row";
    const ts = document.createElement("span");
    ts.className = "ts";
    ts.textContent = formatTimestamp(item.timestamp);
    row2.appendChild(ts);
    if (kind === "rx" && item.callsign) {
      const cs = document.createElement("span");
      cs.className = "callsign";
      cs.textContent = item.callsign;
      row2.appendChild(cs);
    }
    const sz = document.createElement("span");
    sz.className = "size";
    sz.textContent = formatBytes(item.size_bytes);
    row2.appendChild(sz);
    meta.appendChild(row2);
    card.appendChild(meta);

    // Actions : ↻ Renvoyer (TX & RX = relayage radio-secours) + 🗑 Supprimer.
    const actions = document.createElement("div");
    actions.className = "history-card-actions";
    const relayBtn = document.createElement("button");
    relayBtn.className = "btn-relay";
    relayBtn.textContent = t(kind === "tx" ? "history.btn_relay_tx" : "history.btn_relay_rx");
    relayBtn.title = t(kind === "tx" ? "history.relay_tx_tip" : "history.relay_rx_tip");
    const relayPath = kind === "tx" ? item.file_path : item.relay_path;
    relayBtn.addEventListener("click", () => emit("tx:relay", relayPath));
    actions.appendChild(relayBtn);

    // TX cards only: resume the SAME session and continue its fountain
    // (top up partial / late recipients). Distinct from "Relais", which
    // starts a fresh full re-transmission.
    if (kind === "tx") {
      const resumeBtn = document.createElement("button");
      resumeBtn.className = "btn-resume";
      resumeBtn.textContent = t("history.btn_resume_tx");
      resumeBtn.title = t("history.resume_tx_tip");
      resumeBtn.addEventListener("click", () => emit("tx:resume", item.file_path));
      actions.appendChild(resumeBtn);
    }

    const delBtn = document.createElement("button");
    delBtn.className = "btn-delete";
    delBtn.textContent = "🗑";
    delBtn.title = t("history.delete_tip");
    delBtn.addEventListener("click", () => {
      const label = item.filename || t("history.delete_default");
      if (!confirm(t("history.delete_confirm", { what: label }))) return;
      const key = kind === "tx" ? item.file_path : item.session_id;
      deleteHistoryItem(kind, key);
    });
    actions.appendChild(delBtn);
    card.appendChild(actions);

    list.appendChild(card);
  }
}

export async function deleteHistoryItem(kind, key) {
  if (!window.__TAURI__ || !window.__TAURI__.core) return;
  try {
    await invoke("delete_history_item", { kind, key });
    await refreshHistory();
  } catch (err) {
    logEvent("history_delete_error", { kind, key, message: String(err) });
    alert(`Suppression impossible : ${err}`);
  }
}
