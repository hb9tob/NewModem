//! Leaving the application, and offering the modem journal to the collector.
//!
//! Two rules come before everything else here:
//!
//! - **Nothing is sent without a click.** Closing, timing out, failing or
//!   crashing never uploads.
//! - **The application can always be closed.** A frontend that does not
//!   answer within [`ACK_TIMEOUT`], or a second close request, exits at
//!   once, and a watchdog ends the process if the event loop is wedged.
//!
//! The frontend names a journal by its directory name, matched against the
//! store's own listing: it cannot point an upload at another directory.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use modem_worker::journal::store;
use tauri::{AppHandle, CloseRequestApi, Emitter, Manager};

use crate::collector_client::{self, SubmitResult};
use crate::{modem_journal, release_ptt_sounder, settings, AppState};

/// How long the frontend has to say that it shows the dialog.
const ACK_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest wait for RX and TX to stop when leaving.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Ends the process if the event loop never takes the exit request.
const WATCHDOG: Duration = Duration::from_secs(10);
const PREVIEW_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Running,
    /// Waiting for the frontend to acknowledge the request with this number.
    Asked(u64),
    /// The dialog is on screen.
    Shown,
    /// The operator decided to leave: no way back.
    Quitting,
}

#[derive(Debug, PartialEq, Eq)]
enum OnClose {
    Ask(u64),
    Exit,
}

/// State of the exit, kept in [`AppState`].
pub struct ExitControl {
    phase: Mutex<Phase>,
    next_request: AtomicU64,
    /// RX and TX must not start again.
    quitting: Arc<AtomicBool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ExitControl {
    pub fn new() -> Self {
        Self {
            phase: Mutex::new(Phase::Running),
            next_request: AtomicU64::new(1),
            quitting: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn quitting_flag(&self) -> Arc<AtomicBool> {
        self.quitting.clone()
    }

    pub fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::SeqCst)
    }

    /// For the commands that would start RX or TX.
    pub fn ensure_running(&self) -> Result<(), String> {
        if self.is_quitting() {
            Err("fermeture en cours".into())
        } else {
            Ok(())
        }
    }

    fn close_requested(&self, worth_asking: impl FnOnce() -> bool) -> OnClose {
        let mut phase = lock(&self.phase);
        if *phase == Phase::Running && worth_asking() {
            let request = self.next_request.fetch_add(1, Ordering::Relaxed);
            *phase = Phase::Asked(request);
            return OnClose::Ask(request);
        }
        *phase = Phase::Quitting;
        OnClose::Exit
    }

    /// True if nobody answered `request`: leave.
    fn ack_timed_out(&self, request: u64) -> bool {
        let mut phase = lock(&self.phase);
        if *phase != Phase::Asked(request) {
            return false;
        }
        *phase = Phase::Quitting;
        true
    }

    fn acknowledge(&self) -> bool {
        let mut phase = lock(&self.phase);
        if !matches!(*phase, Phase::Asked(_)) {
            return false;
        }
        *phase = Phase::Shown;
        true
    }

    fn cancel(&self) -> bool {
        let mut phase = lock(&self.phase);
        if !matches!(*phase, Phase::Asked(_) | Phase::Shown) {
            return false;
        }
        *phase = Phase::Running;
        true
    }

    /// The operator chose to send and leave, or retries after a failure.
    fn confirm(&self) -> bool {
        let mut phase = lock(&self.phase);
        if !matches!(*phase, Phase::Shown | Phase::Quitting) {
            return false;
        }
        *phase = Phase::Quitting;
        true
    }
}

/// Close button, Alt+F4 and the kiosk exit button all land here, on the
/// main thread.
pub fn on_close_requested(app: &AppHandle, api: &CloseRequestApi) {
    let Some(state) = app.try_state::<AppState>() else { return };
    api.prevent_close();
    let decision = state.exit.close_requested(|| {
        state.journal.had_activity()
            && settings::load().journal_prompt_on_exit
            && !settings::collector_disabled()
    });
    match decision {
        OnClose::Exit => exit_now(app.clone(), false),
        OnClose::Ask(request) => {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
            let _ = app.emit("journal_exit_prompt", ());
            let app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(ACK_TIMEOUT);
                if app.state::<AppState>().exit.ack_timed_out(request) {
                    eprintln!("[journal] no exit dialog, closing without sending");
                    exit_now(app.clone(), false);
                }
            });
        }
    }
}

/// Last stop for an exit that did not go through [`exit_now`].
pub fn on_exit(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        state.journal.close(Duration::from_secs(1));
    }
}

fn exit_now(app: AppHandle, handled: bool) {
    std::thread::spawn(|| {
        std::thread::sleep(WATCHDOG);
        eprintln!("[journal] event loop did not exit, ending the process");
        std::process::exit(0);
    });
    std::thread::spawn(move || {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.hide();
        }
        teardown(&app);
        let state = app.state::<AppState>();
        state.journal.close(CLOSE_TIMEOUT);
        if handled {
            if let Some(dir) = state.journal.dir() {
                if let Err(e) = store::mark_handled(dir) {
                    eprintln!("[journal] cannot mark handled: {e}");
                }
            }
        }
        app.exit(0);
    });
}

/// Release the PTT, stop TX and RX. Safe to call twice.
///
/// The PTT port stays open until the process ends: closing it drops RTS and
/// DTR, which keys a radio wired "low = TX".
fn teardown(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.exit.quitting.store(true, Ordering::SeqCst);

    let (done_tx, done_rx) = mpsc::channel();
    let worker_app = app.clone();
    std::thread::spawn(move || {
        let state = worker_app.state::<AppState>();
        let tx = lock(&state.tx_handle).take();
        if let Some(tx) = &tx {
            tx.stop.store(true, Ordering::Relaxed);
        }
        release_ptt_sounder(&state.ptt);
        let tune = lock(&state.tune_stop).take();
        if let Some(stop) = &tune {
            stop.store(true, Ordering::Relaxed);
        }
        let session = lock(&state.session).take();
        if let Some(session) = session {
            session.capture.stop();
            session.worker.stop();
        }
        lock(&state.radio).take();
        let sounder = lock(&state.sounder_capture).take();
        if let Some(sounder) = sounder {
            sounder.capture.stop();
            if let Some(writer) = sounder.writer_thread {
                let _ = writer.join();
            }
        }
        let recording = lock(&state.wav_sink).take();
        if let Some(recording) = recording {
            let _ = recording.finalize();
        }
        if let Some(tx) = tx {
            tx.stop();
        }
        if tune.is_some() {
            // The carrier thread switches the Pluto TX off as it ends.
            std::thread::sleep(Duration::from_millis(500));
        }
        release_ptt_sounder(&state.ptt);
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(TEARDOWN_TIMEOUT).is_err() {
        eprintln!("[journal] RX or TX still stopping, leaving anyway");
    }
}

/// What the dialog shows about one journal.
#[derive(serde::Serialize)]
pub struct JournalInfo {
    id: String,
    /// Uncompressed.
    bytes: u64,
    callsign: String,
    callsign_ok: bool,
    /// Empty when the operator disabled the collector.
    collector: String,
}

fn info(dir: &std::path::Path) -> Option<JournalInfo> {
    let cfg = settings::load();
    let callsign = cfg.callsign.trim().to_uppercase();
    Some(JournalInfo {
        id: dir.file_name()?.to_str()?.to_string(),
        bytes: store::session_bytes(dir),
        callsign_ok: collector_client::checked_callsign(&callsign).is_ok(),
        callsign,
        collector: if settings::collector_disabled() { String::new() } else { cfg.collector_url },
    })
}

fn resolve(id: &str) -> Result<PathBuf, String> {
    store::sessions(&modem_journal::root())
        .into_iter()
        .find(|dir| dir.file_name().and_then(|n| n.to_str()) == Some(id))
        .ok_or_else(|| "journal introuvable".to_string())
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| format!("journal : {e}"))
}

fn current_info(app: &AppHandle) -> Option<JournalInfo> {
    let journal = app.state::<AppState>().journal.clone();
    journal.flush(Duration::from_secs(1));
    info(journal.dir()?)
}

async fn upload(dir: PathBuf, notes: &str) -> Result<SubmitResult, String> {
    if settings::collector_disabled() {
        return Err("URL collecteur vide (Paramètres → Collecteur)".into());
    }
    let cfg = settings::load();
    collector_client::submit_journal(dir, &cfg.callsign, &cfg.collector_url, notes).await
}

#[tauri::command]
pub async fn journal_exit_ack(app: AppHandle) -> Result<JournalInfo, String> {
    if !app.state::<AppState>().exit.acknowledge() {
        return Err("aucune fermeture en attente".into());
    }
    blocking(move || current_info(&app))
        .await?
        .ok_or_else(|| "journal indisponible".to_string())
}

#[tauri::command]
pub async fn journal_exit_cancel(app: AppHandle) -> Result<bool, String> {
    Ok(app.state::<AppState>().exit.cancel())
}

#[tauri::command]
pub async fn journal_exit_send(app: AppHandle, notes: String) -> Result<SubmitResult, String> {
    let dir = {
        let state = app.state::<AppState>();
        let dir = state
            .journal
            .dir()
            .ok_or_else(|| "journal indisponible".to_string())?
            .to_path_buf();
        collector_client::checked_callsign(&settings::load().callsign)?;
        if !state.exit.confirm() {
            return Err("aucune fermeture en attente".into());
        }
        dir
    };
    let closing = app.clone();
    blocking(move || {
        teardown(&closing);
        closing.state::<AppState>().journal.close(CLOSE_TIMEOUT);
    })
    .await?;
    let result = upload(dir.clone(), &notes).await?;
    if let Err(e) = store::mark_handled(&dir) {
        eprintln!("[journal] cannot mark handled: {e}");
    }
    Ok(result)
}

/// `handled`: sent or declined, do not offer it again.
#[tauri::command]
pub async fn journal_exit_quit(app: AppHandle, handled: bool) -> Result<(), String> {
    *lock(&app.state::<AppState>().exit.phase) = Phase::Quitting;
    exit_now(app, handled);
    Ok(())
}

#[tauri::command]
pub async fn journal_current(app: AppHandle) -> Result<Option<JournalInfo>, String> {
    blocking(move || current_info(&app)).await
}

#[tauri::command]
pub async fn journal_previous() -> Result<Option<JournalInfo>, String> {
    blocking(|| store::previous(&modem_journal::root()).and_then(|dir| info(&dir))).await
}

/// The newest session that did not shut down cleanly and was never answered.
#[tauri::command]
pub async fn journal_crashed() -> Result<Option<JournalInfo>, String> {
    blocking(|| {
        if settings::collector_disabled() {
            return None;
        }
        store::unfinished(&modem_journal::root())
            .last()
            .and_then(|dir| info(dir))
    })
    .await
}

#[tauri::command]
pub async fn journal_send(app: AppHandle, id: String, notes: String) -> Result<SubmitResult, String> {
    let dir = resolve(&id)?;
    let journal = app.state::<AppState>().journal.clone();
    let live = journal.dir() == Some(dir.as_path());
    if live {
        blocking(move || journal.flush(Duration::from_secs(2))).await?;
    }
    let result = upload(dir.clone(), &notes).await?;
    if !live {
        if let Err(e) = store::mark_handled(&dir) {
            eprintln!("[journal] cannot mark handled: {e}");
        }
    }
    Ok(result)
}

#[tauri::command]
pub async fn journal_decline(id: String) -> Result<(), String> {
    store::mark_handled(&resolve(&id)?).map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct JournalPreview {
    /// For this operator's eyes: never part of a journal.
    dir: String,
    text: String,
}

#[tauri::command]
pub async fn journal_preview(id: String) -> Result<JournalPreview, String> {
    blocking(move || {
        let dir = resolve(&id)?;
        let text = store::preview(&dir, PREVIEW_BYTES).map_err(|e| e.to_string())?;
        Ok(JournalPreview {
            dir: dir.to_string_lossy().into_owned(),
            text,
        })
    })
    .await?
}

/// Not `save_settings`: the exit dialog may run before the form is filled.
#[tauri::command]
pub async fn journal_set_prompt(enabled: bool) -> Result<(), String> {
    let mut cfg = settings::load();
    cfg.journal_prompt_on_exit = enabled;
    // `load` fills an empty URL in; keep the collector disabled if it was.
    if settings::collector_disabled() {
        cfg.collector_url.clear();
    }
    settings::save(&cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked() -> (ExitControl, u64) {
        let exit = ExitControl::new();
        match exit.close_requested(|| true) {
            OnClose::Ask(request) => (exit, request),
            OnClose::Exit => panic!("no dialog"),
        }
    }

    #[test]
    fn nothing_worth_asking_exits_at_once() {
        assert_eq!(ExitControl::new().close_requested(|| false), OnClose::Exit);
    }

    #[test]
    fn second_close_always_exits() {
        let (exit, _) = asked();
        assert_eq!(exit.close_requested(|| true), OnClose::Exit);

        let (exit, _) = asked();
        assert!(exit.acknowledge());
        assert_eq!(exit.close_requested(|| true), OnClose::Exit);

        let (exit, _) = asked();
        assert!(exit.acknowledge() && exit.confirm());
        assert_eq!(exit.close_requested(|| true), OnClose::Exit);
    }

    #[test]
    fn silent_frontend_exits_after_the_timeout() {
        let (exit, request) = asked();
        assert!(exit.ack_timed_out(request));
        assert!(!exit.acknowledge(), "late dialog after the exit started");
    }

    #[test]
    fn an_answered_request_never_times_out() {
        let (exit, request) = asked();
        assert!(exit.acknowledge());
        assert!(!exit.ack_timed_out(request));
        assert!(exit.cancel());
        assert!(!exit.ack_timed_out(request));
    }

    #[test]
    fn an_old_timer_does_not_kill_a_new_request() {
        let (exit, first) = asked();
        assert!(exit.cancel());
        let OnClose::Ask(second) = exit.close_requested(|| true) else { panic!("no dialog") };
        assert!(!exit.ack_timed_out(first));
        assert!(exit.ack_timed_out(second));
    }

    #[test]
    fn sending_needs_the_dialog_and_allows_a_retry() {
        assert!(!ExitControl::new().confirm(), "sent without a dialog");
        let (exit, _) = asked();
        assert!(!exit.confirm(), "sent before the dialog was shown");
        assert!(exit.acknowledge() && exit.confirm() && exit.confirm());
        assert!(!exit.cancel(), "cancelled after RX and TX were stopped");
    }
}
