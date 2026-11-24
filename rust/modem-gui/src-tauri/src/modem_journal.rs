//! The modem journal of the GUI process: where it lives, what its header
//! says, and the probe of the main thread.
//!
//! The journal may be published (see `journal_exit`): no path, file name,
//! device name or serial number, URL or free-form error text goes in.

use std::path::PathBuf;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

use modem_worker::journal::{sanitize, store::Limits, Journal};
use serde_json::{Map, Value};
use tauri::AppHandle;

use crate::settings::{self, Settings};

const MAIN_THREAD_PROBE_EVERY: Duration = Duration::from_secs(1);
/// Below this, a late main thread is not worth a record.
const MAIN_THREAD_LATE: Duration = Duration::from_millis(50);

/// For the panic hook, which has no access to the Tauri state.
static CURRENT: OnceLock<Journal> = OnceLock::new();

pub fn root() -> PathBuf {
    if let Some(root) = settings::portable_root() {
        return root.join("journals");
    }
    // Local rather than roaming: up to 100 MB that only matter on this host.
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nbfm-modem-gui")
        .join("journals")
}

/// Open the journal of this session. A journal that cannot open never
/// prevents the modem from running.
pub fn open(app: &AppHandle, cfg: &Settings) -> Journal {
    let mut about = Map::new();
    about.insert("version".into(), app.package_info().version.to_string().into());
    about.insert("commit".into(), env!("NEWMODEM_COMMIT").into());
    about.insert("debug_build".into(), cfg!(debug_assertions).into());
    about.insert(
        "settings".into(),
        serde_json::json!({
            "rx_turbo": cfg.rx_turbo,
            "legacy_grid": cfg.rx_allow_legacy_grid,
            "deemphasis": cfg.rx_deemphasis_enabled,
            "force_mode": cfg.rx_force_mode,
            "full_duplex": cfg.full_duplex_enabled,
            "scrambler": cfg.scrambler_enabled,
            "audio_backend": sanitize::text(&cfg.audio_backend),
            "tx_mode": sanitize::text(&cfg.tx_mode),
            "tx_repair_pct": cfg.tx_repair_pct,
            "tx_speed": cfg.tx_speed,
            "tx_quality": cfg.tx_quality,
            "tx_att_db": cfg.tx_attenuation_db,
            "tx_preemphasis": cfg.tx_preemphasis_enabled,
            "tx_save_wav": cfg.tx_save_wav,
        }),
    );
    let journal = match Journal::open(&root(), about, Limits::default()) {
        Ok(journal) => journal,
        Err(e) => {
            eprintln!("[journal] disabled, cannot open: {e}");
            return Journal::disabled();
        }
    };
    let _ = CURRENT.set(journal.clone());
    spawn_main_thread_probe(app.clone(), journal.clone());
    journal
}

/// `soundcard` or the SDR backend id, never the device.
pub fn device_kind(device_name: &str) -> String {
    crate::sdr_registry::parse_composite_name(device_name)
        .map_or_else(|| "soundcard".to_string(), |(backend, _)| sanitize::text(backend.id()))
}

/// What feeds the RX worker about to start.
pub fn record_rx_source(journal: &Journal, source: &str, audio_backend: Option<&str>) {
    let mut f = Map::new();
    f.insert("source".into(), source.into());
    if let Some(backend) = audio_backend {
        f.insert("audio_backend".into(), sanitize::text(backend).into());
    }
    journal.record("rx_source", None, f);
}

/// Where the panic happened, without its message (it may embed a path).
pub fn on_panic(info: &std::panic::PanicHookInfo<'_>) {
    let Some(journal) = CURRENT.get() else { return };
    let mut f = Map::new();
    if let Some(at) = info.location() {
        // Source file name only: the full path shows the builder's home.
        let file = at.file().rsplit(['/', '\\']).next().unwrap_or("?");
        f.insert("reason".into(), sanitize::text(&format!("{file}:{}", at.line())).into());
    }
    journal.record("panic", None, f);
    journal.flush(Duration::from_millis(500));
}

/// Commands that are not `async` run on the main thread and freeze the
/// window, the close button and `restart_capture`: measure for how long.
fn spawn_main_thread_probe(app: AppHandle, journal: Journal) {
    let spawned = std::thread::Builder::new()
        .name("journal-main-thread".into())
        .spawn(move || loop {
            std::thread::sleep(MAIN_THREAD_PROBE_EVERY);
            let (tx, rx) = mpsc::channel();
            let posted = Instant::now();
            let reply = move || {
                let _ = tx.send(());
            };
            if app.run_on_main_thread(reply).is_err() {
                return;
            }
            if rx.recv().is_err() {
                return;
            }
            let late = posted.elapsed();
            if late >= MAIN_THREAD_LATE {
                let mut f = Map::new();
                f.insert("main_thread_ms".into(), Value::from(late.as_millis() as u64));
                journal.record("main_thread", None, f);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[journal] no main-thread probe: {e}");
    }
}
