//! PTT (Push-To-Talk) controller — wraps a serial port whose RTS/DTR
//! lines drive the radio's TX/RX switch.
//!
//! Pure mechanism, no UI / Settings coupling: the front-end (GUI, future
//! CLI) is responsible for building a `PttConfig` from whatever
//! configuration source it owns (in modem-gui that's `crate::ptt::config_from_settings`).
//!
//! Lifecycle: open the port -> apply RX polarity (set_rx). Before
//! transmitting, switch to TX (set_tx) and wait `PTT_GUARD_MS`. After
//! the WAV finishes (and a symmetric guard), switch back to RX. Drop the
//! controller to release the OS-level port.

use serialport::SerialPort;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Slot shared between the front-end's app state and the TX worker.
/// `None` = PTT disabled for the session (not configured, or open failed).
pub type SharedPtt = Arc<Mutex<Option<PttController>>>;

/// Enumerate serial ports visible to the OS. Returns a list of names
/// (`COM3`, `/dev/ttyUSB0`, ...).
pub fn list_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|v| v.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}
