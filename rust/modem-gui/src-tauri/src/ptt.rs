//! GUI-side PTT glue: bridges the user's `Settings` to the
//! `modem_worker::ptt` controller.
//!
//! On startup and on every settings save we try to open the port and
//! apply the "RX" polarity. The TX worker (in modem-worker) flips the
//! polarity around playback.

use modem_worker::ptt::{PttConfig, PttController};

pub use modem_worker::ptt::{list_ports, SharedPtt, PTT_GUARD_MS};

use crate::settings::Settings;

/// Build a `PttConfig` from the persisted settings, or `None` when the
/// user has disabled PTT (checkbox off, no port chosen, or no line
/// selected).
pub fn config_from_settings(s: &Settings) -> Option<PttConfig> {
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
    let Some(cfg) = config_from_settings(settings) else {
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
