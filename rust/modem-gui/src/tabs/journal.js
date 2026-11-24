// Modem journal — the dialog that offers it to the collector (on exit, after
// a crash, or from Paramètres) and its Paramètres block. The Rust side owns
// the exit: it only waits for `journal_exit_ack` (3 s) and leaves by itself
// otherwise, so nothing here may be required to close the application.
//
// Journal text, paths and server answers are untrusted: textContent only.
import { invoke, listen } from "../lib/ipc.js";
import { t } from "../i18n.js";
import { currentSettings } from "../lib/state.js";
import { formatBytes } from "../lib/format.js";
import { logEvent } from "../lib/log.js";

// Measured on real journals, only to announce a size.
const GZIP_RATIO = 7.5;
const QUIT_AFTER_SENT_MS = 1500;

// `kind`: "exit" | "crash" | "manual".
// `step`: "ask" | "sending" | "failed" | "sent".
let dlg = null;
let ticker = null;

const $ = (id) => document.getElementById(id);

function stopTicker() {
  if (ticker) clearInterval(ticker);
  ticker = null;
}

function open(kind, info) {
  stopTicker();
  dlg = { kind, info, step: "ask", error: null, url: "", seconds: 0 };
  $("journal-notes").value = "";
  $("journal-no-ask").checked = false;
  $("journal-preview").hidden = true;
  $("journal-preview-text").textContent = "";
  $("journal-preview-dir").textContent = "";
  $("journal-modal").hidden = false;
  render();
  // Never the send button: Enter must not publish.
  $("journal-cancel").focus();
}

function close() {
  stopTicker();
  dlg = null;
  $("journal-modal").hidden = true;
}

function canSend() {
  const info = dlg && dlg.info;
  return !!(info && info.callsign_ok && info.collector);
}

function statusText() {
  const { info, step } = dlg;
  if (step === "sending") return t("journal.sending", { s: dlg.seconds });
  if (step === "sent") return t("journal.sent", { url: dlg.url });
  if (step === "failed") return t("journal.failed", { err: dlg.error });
  if (dlg.error) return t("journal.unavailable", { err: dlg.error });
  if (!info) return "";
  if (!info.callsign_ok) return t("journal.no_callsign");
  if (!info.collector) return t("journal.no_collector");
  return "";
}

function render() {
  if (!dlg) return;
  const { kind, info, step } = dlg;
  const asking = step === "ask";
  const exit = kind === "exit";

  $("journal-title").textContent = t(`journal.title_${kind}`);
  $("journal-summary").textContent = info
    ? t("journal.summary", {
        callsign: info.callsign || "—",
        size: formatBytes(info.bytes),
        gz: formatBytes(Math.round(info.bytes / GZIP_RATIO)),
        collector: info.collector || "—",
      })
    : "";

  $("journal-notes").disabled = !(asking || step === "failed");
  $("journal-no-ask-row").hidden = !(exit && asking);
  $("journal-view").disabled = !info;

  const send = $("journal-send");
  send.hidden = step === "sent";
  send.disabled = step === "sending" || !canSend();
  send.textContent = t(step === "failed" ? "journal.retry" : exit ? "journal.send_quit" : "journal.send");

  const decline = $("journal-decline");
  decline.hidden = !((exit && step !== "sent") || (kind === "crash" && asking));
  // Stays clickable while sending: leaving must always be possible.
  decline.disabled = false;
  decline.textContent = t(!exit ? "journal.decline" : asking ? "journal.quit_without" : "journal.quit");

  const cancel = $("journal-cancel");
  // Once confirmed, RX and TX are stopped: leaving is the only way out.
  cancel.hidden = exit && !asking;
  cancel.textContent = t(
    exit ? "journal.cancel" : kind === "crash" && step !== "sent" ? "journal.later" : "journal.close",
  );

  $("journal-status").textContent = statusText();
  $("journal-status").classList.toggle("error", step === "failed" || !!dlg.error);
}

async function setPrompt(enabled) {
  currentSettings.journal_prompt_on_exit = enabled;
  const box = $("journal-prompt-enabled");
  if (box) box.checked = enabled;
  try {
    await invoke("journal_set_prompt", { enabled });
  } catch (err) {
    logEvent("journal_set_prompt_error", { message: String(err) });
  }
}

async function applyNoAsk() {
  if (dlg.kind === "exit" && dlg.step === "ask" && $("journal-no-ask").checked) {
    await setPrompt(false);
  }
}

async function send() {
  if (!canSend() || dlg.step === "sending" || dlg.step === "sent") return;
  const current = dlg;
  const notes = $("journal-notes").value;
  const noAsk = applyNoAsk();
  Object.assign(current, { step: "sending", seconds: 0, error: null });
  render();
  stopTicker();
  ticker = setInterval(() => {
    current.seconds += 1;
    if (dlg === current) render();
  }, 1000);
  try {
    await noAsk;
    const result = current.kind === "exit"
      ? await invoke("journal_exit_send", { notes })
      : await invoke("journal_send", { id: current.info.id, notes });
    if (dlg !== current) return;
    current.step = "sent";
    current.url = `${current.info.collector.replace(/\/+$/, "")}${result.url}`;
    if (current.kind === "exit") {
      setTimeout(() => invoke("journal_exit_quit", { handled: true }), QUIT_AFTER_SENT_MS);
    }
  } catch (err) {
    if (dlg !== current) return;
    current.step = "failed";
    current.error = String(err);
  }
  stopTicker();
  render();
}

async function decline() {
  const { kind, info, step } = dlg;
  if (kind === "exit") {
    await applyNoAsk();
    // A failed or interrupted upload is kept, to be sent from Paramètres.
    await invoke("journal_exit_quit", { handled: step === "ask" && !!info });
    return;
  }
  close();
  if (!info) return;
  try {
    await invoke("journal_decline", { id: info.id });
  } catch (err) {
    logEvent("journal_decline_error", { message: String(err) });
  }
}

async function cancel() {
  if (dlg.kind === "exit" && !(await invoke("journal_exit_cancel"))) return;
  close();
}

async function togglePreview() {
  const box = $("journal-preview");
  if (!box.hidden) {
    box.hidden = true;
    return;
  }
  if (!dlg || !dlg.info) return;
  const text = $("journal-preview-text");
  try {
    const preview = await invoke("journal_preview", { id: dlg.info.id });
    $("journal-preview-dir").textContent = t("journal.preview_dir", { dir: preview.dir });
    text.textContent = preview.text;
  } catch (err) {
    text.textContent = String(err);
  }
  box.hidden = false;
  text.scrollTop = text.scrollHeight;
}

async function onExitPrompt() {
  // Acknowledge first: the backend leaves after 3 s of silence.
  try {
    const info = await invoke("journal_exit_ack");
    open("exit", info);
  } catch (err) {
    open("exit", null);
    dlg.error = String(err);
    render();
  }
}

async function openManual(command) {
  const status = $("journal-settings-status");
  status.textContent = "";
  try {
    const info = await invoke(command);
    if (dlg && dlg.kind === "exit") return;
    if (info) open("manual", info);
    else status.textContent = t("journal.none");
  } catch (err) {
    status.textContent = String(err);
  }
}

// First thing in init(): a close request can come before the settings load.
export function setupJournal() {
  $("journal-send").addEventListener("click", send);
  $("journal-decline").addEventListener("click", decline);
  $("journal-cancel").addEventListener("click", cancel);
  $("journal-view").addEventListener("click", togglePreview);
  $("journal-send-current").addEventListener("click", () => openManual("journal_current"));
  $("journal-send-previous").addEventListener("click", () => openManual("journal_previous"));
  $("journal-prompt-enabled").addEventListener("change", (ev) => setPrompt(ev.target.checked));
  document.addEventListener("langchange", render);
  listen("journal_exit_prompt", onExitPrompt);
}

export function applyJournalSettingsToUI() {
  $("journal-prompt-enabled").checked = currentSettings.journal_prompt_on_exit !== false;
}

// A session that did not end cleanly is offered whatever the setting says.
export async function offerCrashedJournal() {
  try {
    const info = await invoke("journal_crashed");
    if (info && !dlg) open("crash", info);
  } catch (err) {
    logEvent("journal_crashed_error", { message: String(err) });
  }
}
