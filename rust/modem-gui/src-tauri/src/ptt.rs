//! PTT (Push-To-Talk) via RTS/DTR on a COM/tty serial port.
//!
//! On startup and on every settings save we try to open the port and apply
//! the "RX" polarity. Before playing back the TX WAV we switch to the "TX"
//! polarity and wait 200 ms. At the end of the WAV (or on stop) we wait
//! 200 ms of silence and then return to the "RX" polarity.

use serialport::SerialPort;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::settings::Settings;

/// Delay between PTT assertion and audio playback (symmetrically, between
/// end of WAV and PTT release). Covers the radio's TX/RX switching time.
pub const PTT_GUARD_MS: u64 = 200;

#[derive(Clone, Debug)]
pub struct PttConfig {
    pub port: String,
    pub use_rts: bool,
    pub use_dtr: bool,
    /// Level of the RTS line when transmitting (true = high).
    pub rts_tx_high: bool,
    /// Level of the DTR line when transmitting (true = high).
    pub dtr_tx_high: bool,
}

impl PttConfig {
    pub fn from_settings(s: &Settings) -> Option<Self> {
        if !s.ptt_enabled || s.ptt_port.trim().is_empty() {
            return None;
        }
        if !s.ptt_use_rts && !s.ptt_use_dtr {
            return None;
        }
        Some(PttConfig {
            port: s.ptt_port.trim().to_string(),
            use_rts: s.ptt_use_rts,
            use_dtr: s.ptt_use_dtr,
            rts_tx_high: s.ptt_rts_tx_high,
            dtr_tx_high: s.ptt_dtr_tx_high,
        })
    }
}

/// Active PTT controller: wraps an open serial-port handle plus the
/// polarity configuration. Held inside `AppState` behind a mutex.
pub struct PttController {
    cfg: PttConfig,
    port: Box<dyn SerialPort>,
}

impl PttController {
    /// Open the port and immediately apply the RX polarity.
    pub fn open(cfg: PttConfig) -> Result<Self, String> {
        // Baudrate is irrelevant for driving RTS/DTR but the builder
        // requires one. Short timeout: we never write data.
        let port = serialport::new(&cfg.port, 9600)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|e| format!("ouverture port '{}' : {}", cfg.port, e))?;
        let mut ctrl = PttController { cfg, port };
        ctrl.set_rx()?;
        Ok(ctrl)
    }

    pub fn port_name(&self) -> &str {
        &self.cfg.port
    }

    pub fn set_tx(&mut self) -> Result<(), String> {
        self.apply(true)
    }

    pub fn set_rx(&mut self) -> Result<(), String> {
        self.apply(false)
    }

    fn apply(&mut self, tx: bool) -> Result<(), String> {
        if self.cfg.use_rts {
            let level = if tx { self.cfg.rts_tx_high } else { !self.cfg.rts_tx_high };
            self.port
                .write_request_to_send(level)
                .map_err(|e| format!("RTS : {e}"))?;
        }
        if self.cfg.use_dtr {
            let level = if tx { self.cfg.dtr_tx_high } else { !self.cfg.dtr_tx_high };
            self.port
                .write_data_terminal_ready(level)
                .map_err(|e| format!("DTR : {e}"))?;
        }
        Ok(())
    }
}

/// Slot shared between `AppState` and the TX worker. `None` = PTT disabled
/// for the session (not configured, or open failed).
pub type SharedPtt = Arc<Mutex<Option<PttController>>>;

/// Enumerate serial ports visible to the OS. Returns a list of names
/// (`COM3`, `/dev/ttyUSB0`, ...).
pub fn list_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|v| v.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}

/// (Re)try to open the port from the current settings. Returns a
/// human-readable status string for the UI (in French, since it is shown
/// to the end user).
///
/// - `Ok(Some(msg))` : PTT active (msg = e.g. "PTT prête sur COM3").
/// - `Ok(None)`      : PTT disabled by configuration (checkbox off, ...).
/// - `Err(msg)`      : open failed - `slot` is left at `None`.
pub fn refresh(slot: &SharedPtt, settings: &Settings) -> Result<Option<String>, String> {
    // Close the previous handle first (drop = release the port at the OS level).
    if let Ok(mut g) = slot.lock() {
        *g = None;
    }
    let Some(cfg) = PttConfig::from_settings(settings) else {
        return Ok(None);
    };
    let port_name = cfg.port.clone();
    match PttController::open(cfg) {
        Ok(ctrl) => {
            if let Ok(mut g) = slot.lock() {
                *g = Some(ctrl);
            }
            Ok(Some(format!("PTT prête sur {port_name}")))
        }
        Err(e) => Err(e),
    }
}
