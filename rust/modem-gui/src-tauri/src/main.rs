#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod collector_client;
mod overlay;
mod ptt;
mod sdr_registry;
mod settings;
mod tx_encode;

use modem_worker::session_store;
use modem_worker::{rx_worker, tx_worker, EventSink};

use modem_io::cpal_capture::{self, CaptureHandle};
use modem_io::devices::{list_input_devices, list_output_devices, DeviceInfo};
use modem_io::{CpalSink, SampleSink};
use modem_sdr::{DeviceDescriptor, SdrCaptureHandle, SdrDevice};
use ptt::SharedPtt;
use settings::Settings;
use modem_worker::tx_worker::TxHandle;
use modem_worker::rx_worker::{SharedWavSink, WavSink, WorkerHandle};
use tx_encode::{compress_avif, compress_zstd, CompressOpts};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

/// Active RX backend. Dropping the variant releases the underlying
/// hardware: capture thread Drop cascades through `SdrCaptureHandle`
/// for SDRs; cpal Stream / WAV pacer Drop for the other paths.
enum CaptureKind {
    /// cpal soundcard input. Used when the device name doesn't parse
    /// as a `<backend_id>:<device_id>` composite.
    Cpal(CaptureHandle),
    /// Any registered SDR backend (Pluto, SDRplay, …). The boxed
    /// device keeps the hardware handle alive while RX is running;
    /// `SdrCaptureHandle` owns the capture thread and stops it on
    /// drop. The worker downstream sees the same 48 kHz mono
    /// `Vec<f32>` stream as the cpal path.
    Sdr(Box<dyn SdrDevice>, SdrCaptureHandle),
    /// Synthetic capture: a paced thread streams f32 samples loaded
    /// from a WAV file through the same `mpsc::Receiver<Vec<f32>>`
    /// interface the worker expects from cpal / SDR backends. Used
    /// to replay captured audio (offline debug, regression testing).
    WavFile {
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    },
}

impl CaptureKind {
    fn stop(self) {
        match self {
            CaptureKind::Cpal(h) => h.stop(),
            CaptureKind::Sdr(dev, cap) => {
                // Stop the capture stream first (its Drop joins the
                // capture thread / calls Uninit), then release the
                // device handle. Explicit drops keep the order
                // unambiguous.
                drop(cap);
                drop(dev);
            }
            CaptureKind::WavFile { stop, thread } => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(t) = thread {
                    let _ = t.join();
                }
            }
        }
    }
}

struct CaptureSession {
    capture: CaptureKind,
    worker: WorkerHandle,
    device_name: String,
    /// Stored so `restart_capture` (0.10.43) can re-spawn with the same
    /// profile the operator originally picked. The worker emits a
    /// `worker_requests_restart` event on every transition back to Idle
    /// (preamble-absence, EOT, brickwall) ; the GUI listens and invokes
    /// `restart_capture`, which builds a fresh capture + worker pair
    /// here.
    profile: Option<String>,
    forced: bool,
}

/// Standalone sound-card → WAV capture used exclusively by the
/// Sounder tab. Independent of [`CaptureSession`] (the modem
/// rx_worker is intentionally stopped while sounding so the cpal/SDR
/// device is free for raw capture; see Canal-tab tab-switch logic).
struct SounderCapture {
    capture: CaptureKind,
    /// Drains the sample receiver into a 16-bit mono WAV. Returns
    /// `(path, samples_written)` when the receiver hangs up (= the
    /// capture handle is dropped on stop).
    writer_thread: Option<std::thread::JoinHandle<(PathBuf, usize)>>,
}

struct AppState {
    session: Mutex<Option<CaptureSession>>,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    tx_source: Arc<Mutex<Option<Vec<u8>>>>,
    /// Path of the payload ready to transmit (`tx_preview.avif` or
    /// `tx_preview.zst`). Filled in by compress_image / compress_file_zstd.
    /// `tx_start` reads this path to drive the CLI.
    tx_payload_path: Arc<Mutex<Option<PathBuf>>>,
    tx_handle: Mutex<Option<TxHandle>>,
    ptt: SharedPtt,
    sounder_capture: Mutex<Option<SounderCapture>>,
}

#[derive(serde::Serialize, Clone)]
struct PttStatusEvent {
    /// "ok": port open, lines in RX. "off": disabled by config.
    /// "error": open failed - `message` has the details.
    state: &'static str,
    message: String,
}

fn default_save_dir() -> PathBuf {
    if let Some(root) = settings::portable_root() {
        return root.join("nbfm-rx");
    }
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nbfm-rx")
}

#[tauri::command]
fn list_audio_devices() -> Result<Vec<DeviceInfo>, String> {
    list_input_devices().map_err(|e| e.to_string())
}

#[tauri::command]
fn list_output_audio_devices() -> Result<Vec<DeviceInfo>, String> {
    list_output_devices().map_err(|e| e.to_string())
}

/// Per-backend descriptor shipped to the GUI frontend at startup.
/// Each compiled-in `SdrBackend` produces one entry; the frontend
/// caches the list keyed by `id` and reads `capabilities` to
/// dynamically build the per-backend RX/TX panels (frequency input
/// min/max, AGC `<select>`, antenna selector, feature toggles, …).
/// Replaces the legacy per-backend `list_pluto_devices` /
/// `list_sdrplay_devices` commands.
#[derive(Debug, Clone, serde::Serialize)]
struct SdrBackendInfo {
    id: String,
    display_name: String,
    capabilities: modem_sdr::BackendCapabilities,
}

/// Enumerate every SDR backend compiled into this binary, with its
/// static capabilities. The frontend constructs the device dropdown
/// `<optgroup>` headers and the per-backend control panels from the
/// returned list — no hardcoded knowledge of "Pluto" or "SDRplay" in
/// the frontend.
#[tauri::command]
fn list_sdr_backends() -> Vec<SdrBackendInfo> {
    sdr_registry::registered_backends()
        .iter()
        .map(|b| SdrBackendInfo {
            id: b.id().to_string(),
            display_name: b.display_name().to_string(),
            capabilities: b.capabilities().clone(),
        })
        .collect()
}

/// List the live devices visible to one specific SDR backend.
/// Returns an empty `Vec` (not an error) when the backend itself is
/// reachable but no hardware is plugged in — the GUI surfaces this
/// as an empty group, not a red banner.
#[tauri::command]
fn list_sdr_devices(backend_id: String) -> Result<Vec<DeviceDescriptor>, String> {
    let backend = sdr_registry::backend_by_id(&backend_id).map_err(|e| e.to_string())?;
    backend.list_devices().map_err(|e| e.to_string())
}

/// Return per-device capabilities for the device identified by its
/// composite name (`<backend>:<id>`). The frontend calls this each
/// time the user picks a device in the dropdown so the panel layout
/// (antenna selector, tuner radio buttons, bias-T checkbox, gain
/// table size, …) tracks the actual hardware on the bus instead of
/// the family-level lowest-common-denominator caps.
///
/// Backends without per-device variants fall back to family caps via
/// `SdrBackend::capabilities_for`'s default impl, so this command
/// works uniformly for Pluto / RTL-SDR / future single-flavour
/// backends — same JSON shape, no special-case in the frontend.
#[tauri::command]
fn get_sdr_device_capabilities(
    composite_name: String,
) -> Result<modem_sdr::BackendCapabilities, String> {
    let (backend, device_id) = sdr_registry::parse_composite_name(&composite_name)
        .ok_or_else(|| format!("invalid composite name '{composite_name}'"))?;
    // Re-list devices to find the descriptor with its `hardware_hint`
    // — we only persist composite names in settings, not the full
    // descriptor, so the hint has to come from a fresh enumeration.
    // SDRplay's `list_devices` is a sub-millisecond daemon round-trip;
    // calling it once per device-pick is fine.
    let devices = backend.list_devices().map_err(|e| e.to_string())?;
    let descriptor = devices
        .into_iter()
        .find(|d| d.id == device_id)
        // If the device went away between the dropdown render and
        // this call, fall back to a hint-less descriptor — caps_for
        // returns family caps, which is the safe default.
        .unwrap_or_else(|| {
            DeviceDescriptor::new(
                backend.id(),
                device_id,
                format!("{}:{device_id}", backend.id()),
            )
        });
    Ok(backend.capabilities_for(&descriptor).clone())
}

#[tauri::command]
fn list_modem_profiles() -> Vec<modem_core::traits::ProfileDescriptor> {
    use modem_core::traits::Modem;
    modem_core::v3_modem::V3Modem.list_profiles()
}

#[tauri::command]
fn get_settings() -> Settings {
    settings::load()
}

#[tauri::command]
fn save_settings(
    settings: Settings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // Persist before deciding whether to touch the PTT — even if we
    // skip the refresh, the disk file must be up to date.
    let previous = settings::load();
    settings::save(&settings)?;
    // Re-opening the serial port toggles DTR/RTS for a few ms (OS-level
    // behavior of `serialport-rs`), which keys the radio for a short
    // burst. The frontend calls `save_settings` on every settings change
    // (including AVIF quality / speed sliders), so we only refresh PTT
    // when a PTT-relevant field actually changed. Polarity changes are
    // included because PttController applies them at open time.
    let ptt_changed = previous.ptt_enabled != settings.ptt_enabled
        || previous.ptt_port != settings.ptt_port
        || previous.ptt_use_rts != settings.ptt_use_rts
        || previous.ptt_use_dtr != settings.ptt_use_dtr
        || previous.ptt_rts_tx_high != settings.ptt_rts_tx_high
        || previous.ptt_dtr_tx_high != settings.ptt_dtr_tx_high;
    // Recovery path: if PTT is enabled but no controller is currently
    // open (e.g. startup failed because the port was busy), allow any
    // settings save to retry the open. A failed open doesn't toggle the
    // serial lines, so this stays silent on the radio.
    let needs_recovery = settings.ptt_enabled
        && state
            .ptt
            .lock()
            .ok()
            .map(|g| g.is_none())
            .unwrap_or(false);
    if ptt_changed || needs_recovery {
        let status = compute_ptt_status(&state.ptt, &settings);
        let _ = app.emit("ptt_status", status);
    }
    Ok(())
}

fn compute_ptt_status(slot: &SharedPtt, settings: &Settings) -> PttStatusEvent {
    match ptt::refresh(slot, settings) {
        Ok(Some(msg)) => PttStatusEvent {
            state: "ok",
            message: msg,
        },
        Ok(None) => PttStatusEvent {
            state: "off",
            message: "PTT désactivée".to_string(),
        },
        Err(e) => PttStatusEvent {
            state: "error",
            message: format!("PTT indisponible : {e}"),
        },
    }
}

#[tauri::command]
fn list_serial_ports() -> Vec<String> {
    ptt::list_ports()
}

#[tauri::command]
fn ptt_status(state: State<'_, AppState>) -> PttStatusEvent {
    let settings = settings::load();
    if !settings.ptt_enabled {
        return PttStatusEvent {
            state: "off",
            message: "PTT désactivée".to_string(),
        };
    }
    let active = state
        .ptt
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|c| c.port_name().to_string()));
    match active {
        Some(name) => PttStatusEvent {
            state: "ok",
            message: format!("PTT prête sur {name}"),
        },
        None => PttStatusEvent {
            state: "error",
            message: "PTT configurée mais port indisponible".to_string(),
        },
    }
}

/// Resolve `device_name` to a `SampleSink` for TX. Composite SDR
/// names (`<backend>:<device_id>` produced by the registry) open
/// the device through `SdrBackend::open` and ask for `tx_sink()`;
/// plain cpal device names (the legacy soundcard path) fall back
/// to [`CpalSink`].
///
/// Backends that are RX-only return `None` from `tx_sink()` — we
/// surface that to the GUI as a clean error so the operator knows
/// to pick a different TX device.
fn resolve_tx_sink(device_name: &str, cfg: &Settings) -> Result<Arc<dyn SampleSink>, String> {
    if let Some((backend, device_id)) = sdr_registry::parse_composite_name(device_name) {
        let sdr_cfg = cfg.sdr_config_for(backend.id(), device_id);
        let descriptor = DeviceDescriptor::new(
            backend.id(),
            device_id,
            format!("{}:{device_id}", backend.id()),
        );
        let device = backend
            .open(&descriptor, &sdr_cfg)
            .map_err(|e| format!("{}: {e}", backend.id()))?;
        device
            .tx_sink()
            .ok_or_else(|| format!("{} est RX uniquement", backend.id()))
    } else {
        Ok(Arc::new(CpalSink))
    }
}

#[tauri::command]
fn start_capture(
    device_name: String,
    profile: Option<String>,
    forced: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("capture already running".into());
    }
    let forced = forced.unwrap_or(false);
    let session = build_capture_session(
        &device_name,
        profile.clone(),
        forced,
        &app,
        &state,
    )?;
    *guard = Some(session);
    Ok(())
}

/// Drop the current capture session and re-spawn an identical one
/// (same `device_name + profile + forced`). Triggered by the worker
/// emitting `worker_requests_restart` after a transition back to Idle
/// (preamble-absence, EOT, brickwall). Empirically observed in 0.10.42
/// that in-process resets (`soft_reset_buffer` + mpsc drain) didn't
/// re-arm RX on a fresh signal ; a full stop+start does. This command
/// is the programmatic equivalent of the operator clicking
/// "Stop" then "Start" in the GUI.
#[tauri::command]
fn restart_capture(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|e| e.to_string())?;
    let old = match guard.take() {
        Some(s) => s,
        None => {
            eprintln!("[restart_capture] no active session to restart");
            return Err("no capture session to restart".into());
        }
    };
    let device_name = old.device_name.clone();
    let profile = old.profile.clone();
    let forced = old.forced;
    eprintln!(
        "[restart_capture] stopping current session device={} profile={:?} forced={}",
        device_name, profile, forced
    );
    // Drop the old session : stops the cpal/SDR stream, closes the
    // mpsc, and joins the worker thread. Mirrors what `stop_capture`
    // does end-to-end.
    old.capture.stop();
    old.worker.stop();
    eprintln!("[restart_capture] old session dropped, building new");
    let session = build_capture_session(&device_name, profile, forced, &app, &state)?;
    eprintln!("[restart_capture] new session ready");
    *guard = Some(session);
    Ok(())
}

/// Build a fresh `CaptureSession` (cpal/SDR stream + rx_worker thread)
/// for the given device + profile. Shared by `start_capture` (initial
/// user-triggered start) and `restart_capture` (worker-driven auto
/// restart on Idle transitions).
fn build_capture_session(
    device_name: &str,
    profile: Option<String>,
    forced: bool,
    app: &AppHandle,
    state: &State<'_, AppState>,
) -> Result<CaptureSession, String> {
    let profile_idx = resolve_profile(profile.as_deref().unwrap_or("HIGH"), forced)?;
    let cfg = settings::load();
    // Route on the composite device name. SDR devices arrive as
    // `<backend_id>:<device_id>` (produced by `SdrBackend::list_devices`
    // via `DeviceDescriptor::composite_name`); the registry parses it
    // back into (backend, device_id). Plain strings (cpal device
    // names like "USB Audio (hw:1,0)") fall through to the soundcard
    // path. Every branch emits the same shape:
    // `mpsc::Receiver<Vec<f32>>` at 48 kHz mono — the rx_worker
    // doesn't know which backend produced the samples.
    // Drop counter: only cpal capture exposes one (its bounded mpsc
    // increments on `Full`). SDR backends pre-buffer in-thread so they
    // don't need it — we still pass an Arc to keep `rx_worker::spawn`'s
    // signature uniform; the SDR-side counter just stays at zero and
    // the GUI chip never lights up for that reason.
    let (capture, samples, dropped_samples) =
        if let Some((backend, device_id)) = sdr_registry::parse_composite_name(device_name) {
            let sdr_cfg = cfg.sdr_config_for(backend.id(), device_id);
            let descriptor = DeviceDescriptor::new(
                backend.id(),
                device_id,
                format!("{}:{device_id}", backend.id()),
            );
            let mut device = backend
                .open(&descriptor, &sdr_cfg)
                .map_err(|e| format!("{}: {e}", backend.id()))?;
            let (cap_handle, rx) = device
                .start_rx()
                .map_err(|e| format!("{}: {e}", backend.id()))?;
            (
                CaptureKind::Sdr(device, cap_handle),
                rx,
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
            )
        } else {
            let (h, rx) = cpal_capture::start(device_name)?;
            let dropped = h.dropped_samples.clone();
            (CaptureKind::Cpal(h), rx, dropped)
        };
    let sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app.clone()));
    let worker = rx_worker::spawn(
        samples,
        sink,
        state.save_dir.clone(),
        state.wav_sink.clone(),
        profile_idx,
        forced,
        cfg.rx_deemphasis_enabled,
        cfg.rx_allow_legacy_grid,
        dropped_samples,
    );
    Ok(CaptureSession {
        capture,
        worker,
        device_name: device_name.to_string(),
        profile,
        forced,
    })
}

/// Resolve a profile name (case-insensitive, accepts both
/// `HIGH+`/`HIGHPLUS` style aliases) and reject experimental profiles
/// when `forced=false`. Shared by `start_capture` and
/// `start_capture_from_wav` so the WAV replay path mirrors the live
/// capture path's gating exactly.
fn resolve_profile(name: &str, forced: bool) -> Result<modem_core::profile::ProfileIndex, String> {
    let p = match name.to_uppercase().as_str() {
        "MEGA" => modem_core::profile::ProfileIndex::Mega,
        "HIGH" => modem_core::profile::ProfileIndex::High,
        "NORMAL" => modem_core::profile::ProfileIndex::Normal,
        "ROBUST" => modem_core::profile::ProfileIndex::Robust,
        "ULTRA" => modem_core::profile::ProfileIndex::Ultra,
        "HIGH+" | "HIGHPLUS" => modem_core::profile::ProfileIndex::HighPlus,
        "FAST" => modem_core::profile::ProfileIndex::Fast,
        "HIGH++" | "HIGHPLUSPLUS" => modem_core::profile::ProfileIndex::HighPlusPlus,
        "HIGH56" | "HIGH-56" => modem_core::profile::ProfileIndex::HighFiveSix,
        "HIGH+56" | "HIGHPLUS56" => modem_core::profile::ProfileIndex::HighPlusFiveSix,
        other => return Err(format!("unknown profile '{other}'")),
    };
    if p.is_experimental() && !forced {
        return Err(format!(
            "profil '{}' est expérimental, requiert forced=true",
            p.name()
        ));
    }
    Ok(p)
}

#[derive(serde::Deserialize)]
struct StartCaptureFromWavArgs {
    /// WAV file content as raw bytes (read by the frontend via
    /// `file.arrayBuffer()`). Must be mono, 48 kHz, int8/int16/int24
    /// or float32 — the modem chain only handles 48 kHz audio.
    bytes: Vec<u8>,
    profile: Option<String>,
    forced: Option<bool>,
}

/// Replay a WAV file as if it were live audio: the bytes are decoded
/// into f32 samples and a paced helper thread pushes 500 ms batches
/// through an `mpsc::channel` to the rx_worker. Real-time pacing is
/// important — the worker's silence detection and 1 s scan interval
/// are wall-clock based, so dumping the whole buffer at once would
/// produce one giant scan instead of incremental decodes.
#[tauri::command]
fn start_capture_from_wav(
    args: StartCaptureFromWavArgs,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("capture already running".into());
    }
    let forced = args.forced.unwrap_or(false);
    let profile_idx = resolve_profile(args.profile.as_deref().unwrap_or("HIGH"), forced)?;

    // Parse the WAV from in-memory bytes. hound::WavReader::new takes
    // any `Read`, so a `Cursor<Vec<u8>>` works without writing a temp
    // file.
    let mut reader = hound::WavReader::new(std::io::Cursor::new(args.bytes))
        .map_err(|e| format!("WAV parse: {e}"))?;
    let spec = reader.spec();
    if spec.sample_rate != 48_000 {
        return Err(format!(
            "WAV doit être à 48 kHz (vu : {} Hz)",
            spec.sample_rate
        ));
    }
    if spec.channels != 1 {
        return Err(format!(
            "WAV doit être mono (vu : {} canaux)",
            spec.channels
        ));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            // Same convention as modem-cli's read_wav: scale by
            // 2^(bps-1) so a max-amplitude sample lands on ±1.0.
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 / max_val)
                .collect()
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.unwrap_or(0.0))
            .collect(),
    };
    if samples.is_empty() {
        return Err("WAV vide".into());
    }

    // Build the channel + paced sender. Pacing constants chosen to
    // match what cpal_capture / Pluto produce on a live channel.
    let (tx_chan, rx_chan) = std::sync::mpsc::channel::<Vec<f32>>();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_thread = stop.clone();
    let app_for_done = app.clone();
    let pacer = std::thread::spawn(move || {
        // ~500 ms at 48 kHz → matches rx_worker's BATCH_TARGET_SAMPLES
        // so each delivered batch triggers (at most) one scan.
        const BATCH: usize = 24_000;
        const PERIOD: std::time::Duration = std::time::Duration::from_millis(500);
        let start = std::time::Instant::now();
        let mut batch_idx: u32 = 0;
        let mut i = 0usize;
        while i < samples.len()
            && !stop_thread.load(std::sync::atomic::Ordering::Relaxed)
        {
            let end = (i + BATCH).min(samples.len());
            // tx_chan.send fails when the worker has dropped the
            // receiver (stop_capture scenario) — exit cleanly.
            if tx_chan.send(samples[i..end].to_vec()).is_err() {
                break;
            }
            i = end;
            batch_idx += 1;
            // Pace to next "real-time" tick so the worker's wall-clock
            // gates (silence threshold, scan interval) behave like a
            // live capture.
            let target = start + PERIOD * batch_idx;
            let now = std::time::Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }
        // Drop the sender: the worker's recv() will return Err and
        // the worker thread exits naturally.
        drop(tx_chan);
        // Tell the frontend the playback finished — it can flip the
        // toolbar buttons back without waiting for `stop_capture`.
        let _ = tauri::Emitter::emit(&app_for_done, "wav_playback_done", ());
    });

    let cfg = settings::load();
    let sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app.clone()));
    // WAV replay is a deterministic file-pacer; the in-process channel
    // can't drop samples (the pacer paces wall-clock). Pass an
    // always-zero counter; the rx_realtime event will report
    // `dropped_samples = 0` throughout.
    let worker = rx_worker::spawn(
        rx_chan,
        sink,
        state.save_dir.clone(),
        state.wav_sink.clone(),
        profile_idx,
        forced,
        cfg.rx_deemphasis_enabled,
        cfg.rx_allow_legacy_grid,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
    );
    *guard = Some(CaptureSession {
        capture: CaptureKind::WavFile {
            stop,
            thread: Some(pacer),
        },
        worker,
        device_name: "wav-file".to_string(),
        // WAV-file capture has no meaningful restart path (the file
        // is one-shot) ; record the params anyway so the GUI's stored
        // session state is uniform.
        profile: args.profile.clone(),
        forced,
    });
    Ok(())
}

#[tauri::command]
fn stop_capture(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.session.lock().map_err(|e| e.to_string())?;
    if let Some(session) = guard.take() {
        // Drop the capture first : that closes the stream / cancels
        // the libiio buffer pump and disconnects the mpsc channel, so
        // the worker's recv() returns and the thread exits naturally.
        session.capture.stop();
        session.worker.stop();
    }
    // If a raw recording was still armed, finalize it now so the WAV is
    // closed properly. We don't error out if it fails — audio capture is
    // already stopped.
    if let Ok(mut sink_guard) = state.wav_sink.lock() {
        if let Some(sink) = sink_guard.take() {
            let _ = sink.finalize();
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct RawRecordingStatus {
    path: String,
    samples: u64,
    duration_sec: f64,
}

/// Arm raw-audio capture. Returns the absolute path of the new WAV. Fails if
/// a recording is already in progress.
#[tauri::command]
fn start_raw_recording(state: State<'_, AppState>) -> Result<String, String> {
    let mut sink_guard = state.wav_sink.lock().map_err(|e| e.to_string())?;
    if sink_guard.is_some() {
        return Err("raw recording already in progress".into());
    }
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("capture-{ts}.wav"));
    let sink = WavSink::create(&path).map_err(|e| format!("wav create: {e}"))?;
    let path_str = sink.path.to_string_lossy().into_owned();
    *sink_guard = Some(sink);
    Ok(path_str)
}

/// Finalise raw-audio capture. Returns the WAV path and number of samples
/// written. Fails if no recording was active.
#[tauri::command]
fn stop_raw_recording(state: State<'_, AppState>) -> Result<RawRecordingStatus, String> {
    let mut sink_guard = state.wav_sink.lock().map_err(|e| e.to_string())?;
    let sink = sink_guard
        .take()
        .ok_or_else(|| "no raw recording in progress".to_string())?;
    let (path, samples) = sink.finalize().map_err(|e| format!("wav finalize: {e}"))?;
    Ok(RawRecordingStatus {
        path: path.to_string_lossy().into_owned(),
        samples,
        duration_sec: samples as f64 / 48_000.0,
    })
}

#[tauri::command]
fn is_raw_recording(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state
        .wav_sink
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false))
}

/// Submit a finished raw capture to the Phase D collector. URL and HMAC
/// are handled inside `collector_client`. Async because reqwest is;
/// Tauri 2 handles async commands.
#[tauri::command]
async fn submit_capture(
    args: collector_client::SubmitCaptureArgs,
) -> Result<collector_client::SubmitResult, String> {
    collector_client::submit(args).await
}

/// Upload a finished sounding run (the directory holding signature.json
/// + metadata.json, and optionally capture.wav) to the Phase-D
/// collector. Same HMAC contract as `submit_capture`, but reads its
/// payload from disk rather than re-building it from an event log.
#[tauri::command]
async fn submit_sounding(
    args: collector_client::SubmitSoundingArgs,
) -> Result<collector_client::SubmitResult, String> {
    collector_client::submit_sounding(args).await
}

#[derive(serde::Serialize)]
struct CompressResult {
    preview_path: String,
    source_w: u32,
    source_h: u32,
    actual_w: u32,
    actual_h: u32,
    byte_len: usize,
}

#[derive(serde::Serialize)]
struct TxEstimate {
    /// Audio duration to transmit n_initial (K + repair). Reference
    /// value for the progress bar and the "TX > 5 min" guard.
    duration_s: f64,
    /// Total number of blocks emitted by the initial burst (= K + repair).
    total_blocks: u32,
    /// RaptorQ blocks required to decode (K source symbols).
    k_source: u32,
    /// Blocks actually emitted by the initial TX (= K + repair, redundant
    /// with total_blocks but explicit for the UI).
    n_initial: u32,
    /// Minimum theoretical duration if zero packets were lost (K only).
    duration_s_k: f64,
    /// Duration of a single codeword, used UI-side to derive the "+N%"
    /// duration of the More button.
    seconds_per_cw: f64,
}

#[tauri::command]
fn tx_estimate(
    payload_bytes: usize,
    mode: String,
    callsign: String,
    filename: String,
    repair_pct: Option<u32>,
) -> Result<TxEstimate, String> {
    let plan = tx_worker::tx_plan(
        payload_bytes,
        &mode,
        callsign.len(),
        filename.len(),
        repair_pct.unwrap_or(30),
    )?;
    Ok(TxEstimate {
        duration_s: plan.duration_s_initial,
        total_blocks: plan.n_initial,
        k_source: plan.k_source,
        n_initial: plan.n_initial,
        duration_s_k: plan.duration_s_k,
        seconds_per_cw: plan.seconds_per_cw,
    })
}

#[derive(serde::Deserialize)]
struct TxStartArgs {
    mode: String,
    callsign: String,
    filename: String,
    tx_device: String,
    /// RaptorQ repair-pct chosen in the GUI (0, 5, 10, 20, 30, 50, 100...).
    /// Defaults to 30 when the caller omits it.
    #[serde(default)]
    repair_pct: Option<u32>,
}

#[tauri::command]
fn tx_start(
    args: TxStartArgs,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if tx_guard.is_some() {
        return Err("TX déjà en cours".into());
    }
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let payload_path = state
        .tx_payload_path
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "aucune payload prête (compresse d'abord)".to_string())?;
    if !payload_path.exists() {
        return Err(format!(
            "payload absent ({}), recompresse avant TX",
            payload_path.display()
        ));
    }
    if args.callsign.trim().is_empty() {
        return Err("indicatif vide (Paramètres → Indicatif)".into());
    }
    if args.tx_device.trim().is_empty() {
        return Err("périphérique TX non sélectionné (Paramètres)".into());
    }
    let cfg = settings::load();
    let attenuation_db = cfg.tx_attenuation_db;
    let preemphasis_enabled = cfg.tx_preemphasis_enabled;
    let history_max = cfg.tx_history_max;
    let save_wav_dir = if cfg.tx_save_wav { Some(save_dir.clone()) } else { None };
    let repair_pct = args.repair_pct.unwrap_or(30);
    // Resolve the SampleSink upfront on the Tauri thread. The worker
    // thread only sees an `Arc<dyn SampleSink>` — no per-backend
    // branching inside `tx_worker::run_playback`, no `Option<PlutoConfig>`
    // sneaking through the call chain.
    let audio_sink = resolve_tx_sink(&args.tx_device, &cfg)?;
    let archive_sink = TauriEventSink(app.clone());
    tx_worker::archive_payload(
        &save_dir,
        &payload_path,
        &args.mode,
        &args.filename,
        repair_pct,
        history_max,
        &archive_sink,
    );
    let event_sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app));
    let handle = tx_worker::spawn(
        payload_path,
        args.mode,
        args.callsign.trim().to_uppercase(),
        args.filename,
        args.tx_device,
        save_dir,
        repair_pct,
        attenuation_db,
        preemphasis_enabled,
        audio_sink,
        state.ptt.clone(),
        event_sink,
        save_wav_dir,
    );
    *tx_guard = Some(handle);
    Ok(())
}

#[derive(serde::Deserialize)]
struct TxMoreArgs {
    mode: String,
    callsign: String,
    filename: String,
    tx_device: String,
    esi_start: u32,
    /// Exact number of additional blocks to emit. The UI picks it directly
    /// (dropdown / free input) — no more percentage conversion, so "I'm
    /// missing 5 blocks" translates 1:1 to `count = 5`.
    count: u32,
}

#[tauri::command]
fn tx_more(
    args: TxMoreArgs,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if tx_guard.is_some() {
        return Err("TX déjà en cours".into());
    }
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let payload_path = state
        .tx_payload_path
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "aucune payload prête (compresse d'abord)".to_string())?;
    if !payload_path.exists() {
        return Err(format!(
            "payload absent ({}), recompresse avant TX",
            payload_path.display()
        ));
    }
    if args.callsign.trim().is_empty() {
        return Err("indicatif vide (Paramètres → Indicatif)".into());
    }
    if args.tx_device.trim().is_empty() {
        return Err("périphérique TX non sélectionné (Paramètres)".into());
    }
    if args.count == 0 {
        return Err("choisir un nombre de blocs > 0".into());
    }
    let cfg = settings::load();
    let attenuation_db = cfg.tx_attenuation_db;
    let preemphasis_enabled = cfg.tx_preemphasis_enabled;
    let save_wav_dir = if cfg.tx_save_wav { Some(save_dir.clone()) } else { None };
    let audio_sink = resolve_tx_sink(&args.tx_device, &cfg)?;
    let event_sink: Arc<dyn EventSink> = Arc::new(TauriEventSink(app));
    let handle = tx_worker::spawn_more(
        payload_path,
        args.mode,
        args.callsign.trim().to_uppercase(),
        args.filename,
        args.tx_device,
        save_dir,
        args.esi_start,
        args.count,
        attenuation_db,
        preemphasis_enabled,
        audio_sink,
        state.ptt.clone(),
        event_sink,
        save_wav_dir,
    );
    *tx_guard = Some(handle);
    Ok(())
}

#[tauri::command]
fn list_sessions(state: State<'_, AppState>) -> Result<Vec<session_store::SessionMeta>, String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let store = session_store::SessionStore::new(&save_dir).map_err(|e| e.to_string())?;
    Ok(store.list_all())
}

#[tauri::command]
fn delete_session(session_id: u32, state: State<'_, AppState>) -> Result<(), String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let dir = save_dir
        .join("sessions")
        .join(format!("{session_id:08x}.session"));
    if !dir.exists() {
        return Err(format!("session {session_id:08x} absente"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("rm {}: {e}", dir.display()))
}

// ─────────────────────────────────────────── Onglet Historique
//
// Unified view of TX files (archived at TX launch by
// `tx_worker::archive_payload`) and RX files (sessions decoded by
// `session_store`). Powers the radio-rescue mode: an operator receives
// a file and can re-emit it with one click to forward it further on the
// network.

#[derive(serde::Serialize)]
struct TxHistoryItem {
    timestamp: i64,
    mode: String,
    mime_type: u8,
    filename: String,
    file_path: String,
    is_image: bool,
    size_bytes: u64,
}

#[derive(serde::Serialize)]
struct RxHistoryItem {
    session_id: String,
    timestamp: i64,
    callsign: Option<String>,
    filename: String,
    /// Path for the thumbnail (`asset://`-displayable). Always points to
    /// the decompressed/displayable file when it exists (otherwise the
    /// raw `decoded.<ext>` from the session_dir).
    preview_path: String,
    /// Path to pass to `set_tx_source_from_path` for relaying
    /// (radio rescue). AVIF -> `decoded.avif` (bit-for-bit passthrough);
    /// otherwise the root copy.
    relay_path: String,
    is_image: bool,
    size_bytes: u64,
    mode: String,
    mime_type: u8,
}

#[derive(serde::Deserialize)]
struct TxHistoryMetaRead {
    timestamp: i64,
    mode: String,
    mime_type: u8,
    filename: String,
}

#[tauri::command]
fn list_tx_history(state: State<'_, AppState>) -> Result<Vec<TxHistoryItem>, String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let dir = save_dir.join("tx_history");
    let mut items: Vec<TxHistoryItem> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(items), // dossier absent = historique vide, pas une erreur
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else { continue };
        if !ext.eq_ignore_ascii_case("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(meta) = serde_json::from_str::<TxHistoryMetaRead>(&raw) else { continue };
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Find the twin source file (avif/zst/bin) - not necessarily the
        // same extension as the one derived from mime_type, so we walk
        // the directory.
        let mut payload_path: Option<PathBuf> = None;
        for sib in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = sib.path();
            if p.file_stem().and_then(|s| s.to_str()) == Some(&stem)
                && p.extension().and_then(|s| s.to_str())
                    .map(|e| !e.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
            {
                payload_path = Some(p);
                break;
            }
        }
        let Some(payload) = payload_path else { continue };
        let size_bytes = payload.metadata().map(|m| m.len()).unwrap_or(0);
        let is_image =
            meta.mime_type == modem_framing::app_header::mime::IMAGE_AVIF
                || meta.mime_type == modem_framing::app_header::mime::IMAGE_JPEG
                || meta.mime_type == modem_framing::app_header::mime::IMAGE_PNG;
        items.push(TxHistoryItem {
            timestamp: meta.timestamp,
            mode: meta.mode,
            mime_type: meta.mime_type,
            filename: meta.filename,
            file_path: payload.to_string_lossy().into_owned(),
            is_image,
            size_bytes,
        });
    }
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(items)
}

#[tauri::command]
fn list_rx_history(state: State<'_, AppState>) -> Result<Vec<RxHistoryItem>, String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    let store = session_store::SessionStore::new(&save_dir).map_err(|e| e.to_string())?;
    let mut items: Vec<RxHistoryItem> = Vec::new();
    for meta in store.list_all().into_iter().filter(|m| m.decoded) {
        let mime = meta.mime_type;
        let is_image = mime == modem_framing::app_header::mime::IMAGE_AVIF
            || mime == modem_framing::app_header::mime::IMAGE_JPEG
            || mime == modem_framing::app_header::mime::IMAGE_PNG;
        let display_filename = meta.filename.clone().unwrap_or_else(|| {
            format!("session-{:08x}.bin", meta.session_id)
        });
        let root_copy = save_dir.join(&display_filename);
        // Preview AND relay: we always use the root copy written by
        // `rx_worker::emit_decoded_file`. That is the ONLY version that
        // contains the file extracted from the PayloadEnvelope (= pure
        // AVIF/PNG/ZSTD). The `decoded.<ext>` file in the session_dir
        // contains the raw envelope (header + content) and cannot be
        // used directly as an image or as a TX payload.
        if !root_copy.exists() {
            // Session decoded but root copy missing (manual cleanup or
            // sessions inherited from an older version) - skip it.
            continue;
        }
        let preview = root_copy.clone();
        let relay = root_copy.clone();
        let size_bytes = preview.metadata().map(|m| m.len()).unwrap_or(0);
        items.push(RxHistoryItem {
            session_id: format!("{:08x}", meta.session_id),
            timestamp: meta.created_at as i64,
            callsign: meta.callsign,
            filename: display_filename,
            preview_path: preview.to_string_lossy().into_owned(),
            relay_path: relay.to_string_lossy().into_owned(),
            is_image,
            size_bytes,
            mode: meta.profile,
            mime_type: mime,
        });
    }
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(items)
}

#[tauri::command]
fn delete_history_item(
    kind: String,
    key: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let save_dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    match kind.as_str() {
        "tx" => {
            let path = PathBuf::from(&key);
            // Guard: must be inside <save_dir>/tx_history/.
            let history_dir = save_dir.join("tx_history");
            if !path.starts_with(&history_dir) {
                return Err("chemin hors tx_history/".into());
            }
            // Supprime le fichier + son metadata jumeau (.json).
            let _ = std::fs::remove_file(&path);
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let meta = history_dir.join(format!("{stem}.json"));
                let _ = std::fs::remove_file(&meta);
            }
            Ok(())
        }
        "rx" => {
            // key = session_id hex 8 chars.
            let session_id = u32::from_str_radix(&key, 16)
                .map_err(|e| format!("session_id invalide '{key}': {e}"))?;
            let dir = save_dir
                .join("sessions")
                .join(format!("{session_id:08x}.session"));
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .map_err(|e| format!("rm {}: {e}", dir.display()))?;
            }
            // We also remove the root copy if we can recover the filename
            // from the meta. Best-effort: we don't read meta.json any more
            // (already rm'd) - we leave the root copy in place if the user
            // has already moved or copied it elsewhere; safer than blind
            // deletion.
            Ok(())
        }
        other => Err(format!("kind inconnu '{other}' (tx|rx)")),
    }
}

#[tauri::command]
fn tx_stop(state: State<'_, AppState>) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if let Some(h) = tx_guard.take() {
        h.stop();
    }
    Ok(())
}

/// Called by JS once it has consumed tx_complete/tx_error. Cleans up the
/// handle slot so subsequent tx_start calls work.
#[tauri::command]
fn tx_reset(state: State<'_, AppState>) -> Result<(), String> {
    let mut tx_guard = state.tx_handle.lock().map_err(|e| e.to_string())?;
    if let Some(h) = tx_guard.take() {
        h.stop();
    }
    Ok(())
}

#[tauri::command]
fn set_tx_source(bytes: Vec<u8>, state: State<'_, AppState>) -> Result<usize, String> {
    let len = bytes.len();
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = Some(bytes);
    Ok(len)
}

// Path-based variant used by the drag-drop code path. Passing the raw bytes
// through IPC (set_tx_source) forces JSON-array serialization of the whole
// image, which allocates ~10× the file size between JS and Rust and was
// enough to OOM-freeze the desktop on large drops.
#[tauri::command]
fn set_tx_source_from_path(
    path: String,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
    let len = bytes.len();
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = Some(bytes);
    Ok(len)
}

#[tauri::command]
fn clear_tx_source(state: State<'_, AppState>) -> Result<(), String> {
    let mut slot = state.tx_source.lock().map_err(|e| e.to_string())?;
    *slot = None;
    if let Ok(mut p) = state.tx_payload_path.lock() {
        *p = None;
    }
    Ok(())
}

#[tauri::command]
fn compress_image(
    opts: CompressOpts,
    state: State<'_, AppState>,
) -> Result<CompressResult, String> {
    let source = {
        let slot = state.tx_source.lock().map_err(|e| e.to_string())?;
        slot.clone().ok_or_else(|| "no tx source loaded".to_string())?
    };
    let result = compress_avif(&source, &opts)?;
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("tx_preview.avif");
    std::fs::write(&path, &result.avif_bytes).map_err(|e| format!("write: {e}"))?;
    if let Ok(mut p) = state.tx_payload_path.lock() {
        *p = Some(path.clone());
    }
    Ok(CompressResult {
        preview_path: path.to_string_lossy().into_owned(),
        source_w: result.source_w,
        source_h: result.source_h,
        actual_w: result.actual_w,
        actual_h: result.actual_h,
        byte_len: result.byte_len,
    })
}

#[derive(serde::Serialize)]
struct CompressFileResult {
    preview_path: String,
    source_len: usize,
    byte_len: usize,
}

/// Compress the source (loaded via `set_tx_source_from_path`) with zstd
/// at max level and write the result to `tx_preview.zst`. For non-image
/// files (text, archives, etc.) that require lossless transmission.
#[tauri::command]
fn compress_file_zstd(state: State<'_, AppState>) -> Result<CompressFileResult, String> {
    let source = {
        let slot = state.tx_source.lock().map_err(|e| e.to_string())?;
        slot.clone().ok_or_else(|| "no tx source loaded".to_string())?
    };
    let result = compress_zstd(&source)?;
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("tx_preview.zst");
    std::fs::write(&path, &result.zst_bytes).map_err(|e| format!("write: {e}"))?;
    if let Ok(mut p) = state.tx_payload_path.lock() {
        *p = Some(path.clone());
    }
    Ok(CompressFileResult {
        preview_path: path.to_string_lossy().into_owned(),
        source_len: result.source_len,
        byte_len: result.byte_len,
    })
}

#[tauri::command]
fn overlays_import_logo(bytes: Vec<u8>, original_name: String) -> Result<String, String> {
    overlay::import_logo_bytes(&bytes, &original_name)
}

#[tauri::command]
fn overlays_logos_dir() -> Result<String, String> {
    let dir = overlay::logos_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().into_owned())
}

#[tauri::command]
fn get_save_dir(state: State<'_, AppState>) -> Result<String, String> {
    let dir = state.save_dir.lock().map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().into_owned())
}

#[tauri::command]
fn set_save_dir(path: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut dir = state.save_dir.lock().map_err(|e| e.to_string())?;
    let p = PathBuf::from(path);
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    *dir = p;
    Ok(())
}

// `sounding_tx_render` builds the probe audio + schedule on TX side and
// drops them under `<save_dir>/sounder/<id>/` so the operator can play
// `probe.wav` through their soundcard. `sounding_analyze` consumes a
// recorded capture WAV against the matching schedule and writes a
// `signature.json` ready to feed back into `study/nbfm_channel_sim`.
//
// Pure file-IO wrappers around `modem_worker_base::sounder` — no Tauri
// state involved beyond the configured save_dir; same DSP path the
// CLI's `sounding-analyze` subcommand uses.

#[derive(serde::Serialize)]
struct SoundingTxResult {
    /// Unique run id (`<unix>-<rand>`), also the subdirectory name
    /// under `<save_dir>/sounder/`.
    id: String,
    /// Absolute path of `probe.wav` (48 kHz mono, 16-bit PCM).
    probe_wav: String,
    /// Absolute path of `schedule.json` (the contract the RX side
    /// needs to run `sounding_analyze`).
    schedule_json: String,
    /// Total duration of `probe.wav` in seconds, so the GUI can show
    /// "play, then wait N seconds".
    duration_s: f64,
}

/// Render a probe schedule + WAV from a `SoundingRequest` and drop both
/// under `<save_dir>/sounder/<id>/`. Front-end: TX operator picks
/// probes, calls this, plays `probe.wav` through their soundcard while
/// the RX side records.
#[tauri::command]
fn sounding_tx_render(
    request: modem_worker_base::sounder::SoundingRequest,
    state: State<'_, AppState>,
) -> Result<SoundingTxResult, String> {
    use modem_worker_base::sounder::build_probe_schedule;
    let (audio, schedule) = build_probe_schedule(&request);

    // Land everything under <save_dir>/sounder/<id>/.
    let save_dir = state
        .save_dir
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    let id = format!(
        "{}-{:04x}",
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        rand_u16(),
    );
    let run_dir = save_dir.join("sounder").join(&id);
    std::fs::create_dir_all(&run_dir)
        .map_err(|e| format!("create {}: {e}", run_dir.display()))?;

    let probe_wav = run_dir.join("probe.wav");
    let schedule_json = run_dir.join("schedule.json");

    // Same 16-bit mono encoding the CLI uses.
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: modem_core_base::types::AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&probe_wav, spec)
        .map_err(|e| format!("create {}: {e}", probe_wav.display()))?;
    for &s in &audio {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        w.write_sample(v)
            .map_err(|e| format!("write sample: {e}"))?;
    }
    w.finalize()
        .map_err(|e| format!("finalize {}: {e}", probe_wav.display()))?;

    let sched_bytes = serde_json::to_vec_pretty(&schedule)
        .map_err(|e| format!("serialize schedule: {e}"))?;
    std::fs::write(&schedule_json, &sched_bytes)
        .map_err(|e| format!("write {}: {e}", schedule_json.display()))?;

    let duration_s =
        audio.len() as f64 / modem_core_base::types::AUDIO_RATE as f64;
    Ok(SoundingTxResult {
        id,
        probe_wav: probe_wav.to_string_lossy().into_owned(),
        schedule_json: schedule_json.to_string_lossy().into_owned(),
        duration_s,
    })
}

#[derive(serde::Deserialize)]
struct SoundingTxEmitArgs {
    request: modem_worker_base::sounder::SoundingRequest,
    tx_device: String,
}

#[derive(serde::Serialize)]
struct SoundingTxEmitResult {
    /// Total airtime of the probe sequence in seconds — the front-end
    /// uses this to enable a countdown / disable the button.
    duration_s: f64,
}

/// Emit a probe sequence directly through `tx_device` — no probe.wav
/// on disk, no schedule.json shown to the user. The RX side will
/// regenerate the same schedule locally (deterministic generation,
/// proven by the md5-matches-across-runs property of
/// `build_probe_schedule`).
///
/// PTT discipline: if a serial-port PTT is configured (Paramètres
/// tab), it is engaged before opening the audio stream and released
/// `PTT_GUARD_MS` after the last sample plays — same pattern as
/// `tx_runtime::run_playback`. With PTT disabled the radio is assumed
/// to be VOX-keyed; the 5 s @ 1750 Hz repeater-opening preamble at
/// the start of the probe gives VOX plenty of time to engage.
#[tauri::command]
fn sounding_tx_emit(
    args: SoundingTxEmitArgs,
    state: State<'_, AppState>,
) -> Result<SoundingTxEmitResult, String> {
    use modem_worker_base::sounder::build_probe_schedule;
    if args.tx_device.trim().is_empty() {
        return Err("Choisir un device de sortie audio".into());
    }
    let (audio, _schedule) = build_probe_schedule(&args.request);
    let duration_s =
        audio.len() as f64 / modem_core_base::types::AUDIO_RATE as f64;

    let cfg = settings::load();
    let sink = resolve_tx_sink(&args.tx_device, &cfg)?;
    let device = args.tx_device.clone();
    let ptt_slot = state.ptt.clone();
    // Run the playback on a dedicated thread — the PlaybackHandle wraps
    // a cpal `Stream` whose underlying object is `Box<dyn Any>` (not
    // marked Send), so we can't move it across threads after creation.
    // Instead, we build it ON the worker thread; the Arc<dyn
    // SampleSink> + the audio Vec are both Send.
    std::thread::spawn(move || {
        let ptt_engaged = engage_ptt_sounder(&ptt_slot);
        if ptt_engaged {
            std::thread::sleep(std::time::Duration::from_millis(
                ptt::PTT_GUARD_MS,
            ));
        }
        let handle = match sink.play_buffer(
            &device,
            modem_core_base::types::AUDIO_RATE,
            audio,
        ) {
            Ok(h) => h,
            Err(_) => {
                if ptt_engaged {
                    release_ptt_sounder(&ptt_slot);
                }
                return;
            }
        };
        let mut consecutive_done = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if handle.is_done() {
                consecutive_done += 1;
                if consecutive_done >= 5 {
                    break;
                }
            }
        }
        drop(handle);
        if ptt_engaged {
            std::thread::sleep(std::time::Duration::from_millis(
                ptt::PTT_GUARD_MS,
            ));
            release_ptt_sounder(&ptt_slot);
        }
    });

    Ok(SoundingTxEmitResult { duration_s })
}

/// Mirror of `tx_runtime::ptt_engage` — kept private to that module,
/// so we re-implement the 4-line helper here rather than make it `pub`
/// just for the sounder. Returns `true` iff a controller was present
/// AND `set_tx` succeeded.
fn engage_ptt_sounder(slot: &SharedPtt) -> bool {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let Some(ctrl) = g.as_mut() else { return false };
    match ctrl.set_tx() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[ptt] sounder set_tx: {e}");
            false
        }
    }
}

fn release_ptt_sounder(slot: &SharedPtt) {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(ctrl) = g.as_mut() {
        if let Err(e) = ctrl.set_rx() {
            eprintln!("[ptt] sounder set_rx: {e}");
        }
    }
}

/// Open `device_name` (same composite-name form as `start_capture`:
/// `<backend>:<id>` for SDRs, plain cpal name otherwise) and dump
/// every received sample to a 48 kHz / 16-bit / mono WAV under the
/// configured save_dir. Returns the absolute path of the WAV.
///
/// This is **independent of the rx_worker / modem decoding pipeline**:
/// the Sounder tab intentionally stops the worker before sounding so
/// the cpal/SDR device is free for raw capture.
#[tauri::command]
fn sounding_rx_start_capture(
    device_name: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut guard = state.sounder_capture.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("Capture sondeur déjà active".into());
    }
    if state.session.lock().map(|g| g.is_some()).unwrap_or(false) {
        return Err(
            "Arrêter d'abord la réception (▶) avant d'utiliser le sondeur — \
             le device audio doit être libre"
                .into(),
        );
    }
    if device_name.trim().is_empty() {
        return Err("Sélectionner une carte RX dans Paramètres".into());
    }

    let cfg = settings::load();
    let (capture, samples_rx) = if let Some((backend, device_id)) =
        sdr_registry::parse_composite_name(&device_name)
    {
        let sdr_cfg = cfg.sdr_config_for(backend.id(), device_id);
        let descriptor = DeviceDescriptor::new(
            backend.id(),
            device_id,
            format!("{}:{device_id}", backend.id()),
        );
        let mut device = backend
            .open(&descriptor, &sdr_cfg)
            .map_err(|e| format!("{}: {e}", backend.id()))?;
        let (cap_handle, rx) = device
            .start_rx()
            .map_err(|e| format!("{}: {e}", backend.id()))?;
        (CaptureKind::Sdr(device, cap_handle), rx)
    } else {
        let (h, rx) = cpal_capture::start(&device_name)?;
        (CaptureKind::Cpal(h), rx)
    };

    let dir = state.save_dir.lock().map_err(|e| e.to_string())?.clone();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("capture-{ts}.wav"));
    let path_str = path.to_string_lossy().into_owned();

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: modem_core_base::types::AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec)
        .map_err(|e| format!("create {}: {e}", path.display()))?;

    let path_thread = path.clone();
    let writer_thread = std::thread::spawn(move || {
        let mut total = 0_usize;
        while let Ok(chunk) = samples_rx.recv() {
            for &s in &chunk {
                let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
                let _ = w.write_sample(v);
            }
            total += chunk.len();
        }
        let _ = w.finalize();
        (path_thread, total)
    });

    *guard = Some(SounderCapture {
        capture,
        writer_thread: Some(writer_thread),
    });
    Ok(path_str)
}

/// Stop the standalone sounder capture: drops the [`CaptureKind`]
/// (which closes the cpal/SDR backend → the writer thread's recv()
/// errors out → it finalises the WAV and exits), then joins the
/// writer thread and returns the final path + sample count.
#[tauri::command]
fn sounding_rx_stop_capture(
    state: State<'_, AppState>,
) -> Result<RawRecordingStatus, String> {
    let cap = {
        let mut guard = state
            .sounder_capture
            .lock()
            .map_err(|e| e.to_string())?;
        guard
            .take()
            .ok_or_else(|| "Aucune capture sondeur active".to_string())?
    };
    let SounderCapture {
        capture,
        writer_thread,
    } = cap;
    capture.stop();
    let (path, samples) = if let Some(t) = writer_thread {
        t.join()
            .map_err(|_| "Writer thread panicked".to_string())?
    } else {
        return Err("Pas de writer thread".into());
    };
    Ok(RawRecordingStatus {
        path: path.to_string_lossy().into_owned(),
        samples: samples as u64,
        duration_sec: samples as f64
            / modem_core_base::types::AUDIO_RATE as f64,
    })
}

/// Run the sounder analyser on a recorded capture WAV. Writes
/// `signature.json` next to the capture and returns the signature so
/// the GUI can render the derived parameters directly.
#[tauri::command]
fn sounding_analyze(
    capture_wav: String,
    schedule_json: String,
    family: String,
    metadata: modem_worker_base::sounder::SoundingMetadata,
    sync_threshold: f32,
) -> Result<modem_worker_base::sounder::ChannelSignature, String> {
    use modem_worker_base::sounder::{
        analyze_capture, ChannelFamily, ProbeSchedule,
    };
    let capture_path = PathBuf::from(&capture_wav);
    let schedule_path = PathBuf::from(&schedule_json);

    let sched_bytes = std::fs::read(&schedule_path)
        .map_err(|e| format!("read {}: {e}", schedule_path.display()))?;
    let sched: ProbeSchedule = serde_json::from_slice(&sched_bytes)
        .map_err(|e| format!("parse {}: {e}", schedule_path.display()))?;
    if sched.sample_rate != modem_core_base::types::AUDIO_RATE {
        return Err(format!(
            "schedule sample_rate {} != GUI AUDIO_RATE {}",
            sched.sample_rate,
            modem_core_base::types::AUDIO_RATE,
        ));
    }

    let mut reader = hound::WavReader::open(&capture_path)
        .map_err(|e| format!("open {}: {e}", capture_path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != modem_core_base::types::AUDIO_RATE {
        return Err(format!(
            "capture sample_rate {} != {} Hz",
            spec.sample_rate,
            modem_core_base::types::AUDIO_RATE,
        ));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = ((1u32 << (spec.bits_per_sample - 1)) - 1) as f32;
            reader
                .samples::<i32>()
                .filter_map(Result::ok)
                .map(|s| s as f32 / scale)
                .collect()
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .filter_map(Result::ok)
            .collect(),
    };
    if samples.is_empty() {
        return Err(format!("capture {} is empty", capture_path.display()));
    }
    let mono: Vec<f32> = if spec.channels == 1 {
        samples
    } else {
        samples
            .chunks(spec.channels as usize)
            .map(|c| c[0])
            .collect()
    };

    let fam = match family.to_lowercase().as_str() {
        "fm" => ChannelFamily::Fm,
        "qo100" | "qo-100" => ChannelFamily::Qo100,
        "ssb_hf" | "ssb-hf" | "ssbhf" => ChannelFamily::SsbHf,
        other => {
            return Err(format!(
                "unknown family '{other}' (expected fm | qo100 | ssb_hf)"
            ));
        }
    };

    let sig = analyze_capture(&mono, &sched, fam, metadata, sync_threshold)
        .map_err(|e| format!("analyse: {e}"))?;

    let sig_path = capture_path.with_file_name(
        capture_path
            .file_stem()
            .map(|s| format!("{}.signature.json", s.to_string_lossy()))
            .unwrap_or_else(|| "signature.json".to_string()),
    );
    let sig_bytes = serde_json::to_vec_pretty(&sig)
        .map_err(|e| format!("serialize signature: {e}"))?;
    std::fs::write(&sig_path, &sig_bytes)
        .map_err(|e| format!("write {}: {e}", sig_path.display()))?;

    Ok(sig)
}

/// Tiny 16-bit PRNG for the run-id suffix. Avoids pulling in `rand`
/// just to disambiguate two TX renders inside the same second.
fn rand_u16() -> u16 {
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let probe: u16 = 0xa1b2;
    let addr = &probe as *const _ as usize as u32;
    (nanos.wrapping_mul(2654435761) ^ addr ^ probe as u32) as u16
}

/// Adapter that bridges `modem_worker::EventSink` onto a Tauri `AppHandle`,
/// so workers extracted into `modem-worker` can keep their existing event
/// names + payload shapes without depending on Tauri.
struct TauriEventSink(AppHandle);

impl EventSink for TauriEventSink {
    fn emit_json(&self, name: &str, payload: serde_json::Value) {
        // Same fire-and-forget semantics the workers used to have when
        // they called `app.emit(...)` directly.
        let _ = self.0.emit(name, payload);
    }
}

fn main() {
    // Work around a WebKitGTK + Mesa V3D bug on Raspberry Pi 4/5 where the
    // DMA-BUF renderer leaves the toplevel surface corrupted ("scrambled
    // Canal+" bands) after a fullscreen transition, tab switch, or image
    // upload. Forcing the legacy renderer makes the issue disappear. Must
    // be set before any webview is created, hence at the very top of main.
    #[cfg(target_os = "linux")]
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    let save_dir = default_save_dir();
    let _ = std::fs::create_dir_all(&save_dir);

    // Install the default oscilloscope logo + pre-configure slot 1
    // (top-left corner, 0/0% margin, 10% height) the first time this
    // build runs against a given user profile. Triggers on both fresh
    // installs (no settings.json) and upgrades from a version that did
    // not have overlays. The `overlay_default_seeded` flag prevents
    // re-seeding on every launch and protects user customizations:
    // once set, we never touch slot 1 again. Slot 1 is only filled if
    // it is still empty, and `active_overlay` is only changed if the
    // user has not picked another slot.
    {
        let mut s = settings::load();
        if !s.overlay_default_seeded {
            match overlay::ensure_default_logo() {
                Ok(filename) => {
                    if let Some(slot) = s.overlays.get_mut(1) {
                        if slot.text.is_none() && slot.logo.is_none() {
                            slot.name = "NBFM Modem".to_string();
                            slot.logo = Some(overlay::LogoElement {
                                filename,
                                anchor: overlay::Anchor::TopLeft,
                                margin_x_pct: 0.0,
                                margin_y_pct: 0.0,
                                size_pct: 10.0,
                            });
                            if s.active_overlay == 0 {
                                s.active_overlay = 1;
                            }
                        }
                    }
                    s.overlay_default_seeded = true;
                    if let Err(e) = settings::save(&s) {
                        eprintln!("[overlay] could not save seeded settings: {e}");
                    }
                }
                Err(e) => eprintln!("[overlay] could not write default logo: {e}"),
            }
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let ptt: SharedPtt = Arc::new(Mutex::new(None));
            // Best-effort PTT port open at startup. On failure we emit an
            // event and leave `ptt` at None for the session - the UI can
            // still re-open it later via Settings -> save_settings.
            let startup_settings = settings::load();
            let status = compute_ptt_status(&ptt, &startup_settings);
            if status.state == "error" {
                eprintln!("[ptt] {}", status.message);
            }
            let _ = app.handle().emit("ptt_status", status);
            app.manage(AppState {
                session: Mutex::new(None),
                save_dir: Arc::new(Mutex::new(save_dir.clone())),
                wav_sink: Arc::new(Mutex::new(None)),
                tx_source: Arc::new(Mutex::new(None)),
                tx_payload_path: Arc::new(Mutex::new(None)),
                tx_handle: Mutex::new(None),
                ptt,
                sounder_capture: Mutex::new(None),
            });
            // Auto-kiosk on tiny touchscreens (e.g. Pi 7" 800x480) or
            // when `NBFM_KIOSK=1` is set in the environment. Both paths
            // force a borderless fullscreen window and emit `kiosk_mode`
            // to the frontend so the CSS layout switches and the
            // on-screen exit button shows up. Diagnostic eprintlns are
            // intentional — running the binary from a terminal makes it
            // easy to see why the auto-detection did or didn't engage.
            let env_kiosk = std::env::var("NBFM_KIOSK")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false);
            if let Some(win) = app.get_webview_window("main") {
                let mon_kiosk = match win.primary_monitor() {
                    Ok(Some(monitor)) => {
                        let s = monitor.size();
                        let f = monitor.scale_factor().max(0.01);
                        let lw = (s.width as f64 / f) as u32;
                        let lh = (s.height as f64 / f) as u32;
                        eprintln!(
                            "[kiosk] monitor: phys {}x{} scale {:.2} -> logical {}x{}",
                            s.width, s.height, f, lw, lh
                        );
                        lw <= 900 || lh <= 600
                    }
                    Ok(None) => {
                        eprintln!("[kiosk] no primary monitor reported");
                        false
                    }
                    Err(e) => {
                        eprintln!("[kiosk] primary_monitor error: {e}");
                        false
                    }
                };
                if env_kiosk || mon_kiosk {
                    eprintln!(
                        "[kiosk] engaging (env={} mon={})",
                        env_kiosk, mon_kiosk
                    );
                    if let Err(e) = win.set_decorations(false) {
                        eprintln!("[kiosk] set_decorations err: {e}");
                    }
                    if let Err(e) = win.set_fullscreen(true) {
                        eprintln!("[kiosk] set_fullscreen err: {e}");
                    }
                    // Defer a second attempt: on Wayland (labwc), the
                    // toplevel surface may not be mapped yet when setup
                    // runs, so the first set_fullscreen can be a no-op.
                    let win2 = win.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        let _ = win2.set_decorations(false);
                        let _ = win2.set_fullscreen(true);
                    });
                    let _ = app.handle().emit("kiosk_mode", true);
                } else {
                    eprintln!("[kiosk] desktop mode (env={} mon={})", env_kiosk, mon_kiosk);
                }
            } else {
                eprintln!("[kiosk] no `main` window in setup hook");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_audio_devices,
            list_output_audio_devices,
            list_sdr_backends,
            list_sdr_devices,
            get_sdr_device_capabilities,
            list_modem_profiles,
            list_serial_ports,
            ptt_status,
            get_settings,
            save_settings,
            start_capture,
            start_capture_from_wav,
            stop_capture,
            restart_capture,
            get_save_dir,
            set_save_dir,
            start_raw_recording,
            stop_raw_recording,
            is_raw_recording,
            submit_capture,
            submit_sounding,
            sounding_tx_render,
            sounding_tx_emit,
            sounding_rx_start_capture,
            sounding_rx_stop_capture,
            sounding_analyze,
            set_tx_source,
            set_tx_source_from_path,
            clear_tx_source,
            compress_image,
            compress_file_zstd,
            overlays_import_logo,
            overlays_logos_dir,
            tx_estimate,
            tx_start,
            tx_more,
            tx_stop,
            tx_reset,
            list_sessions,
            delete_session,
            list_tx_history,
            list_rx_history,
            delete_history_item,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
