//! PTT (Push-To-Talk) via RTS/DTR sur port série COM/tty.
//!
//! Au démarrage et à chaque sauvegarde des paramètres on tente d'ouvrir le
//! port et d'appliquer la polarité "RX". Avant la lecture du WAV TX on bascule
//! sur la polarité "TX" et on attend 200 ms. À la fin du WAV (ou stop) on
//! attend 200 ms de silence et on remet la polarité "RX".

use serialport::SerialPort;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::settings::Settings;

/// Délai entre l'assertion PTT et le démarrage audio (et symétriquement entre
/// la fin du WAV et la libération PTT). Couvre le temps de commutation
/// TX/RX de la radio.
pub const PTT_GUARD_MS: u64 = 200;

#[derive(Clone, Debug)]
pub struct PttConfig {
    pub port: String,
    pub use_rts: bool,
    pub use_dtr: bool,
    /// Niveau de la ligne RTS quand on est en émission (true = haut).
    pub rts_tx_high: bool,
    /// Niveau de la ligne DTR quand on est en émission (true = haut).
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

/// Contrôleur PTT actif : encapsule un handle de port série ouvert et la
/// configuration de polarité. On le garde dans `AppState` derrière un mutex.
pub struct PttController {
    cfg: PttConfig,
    port: Box<dyn SerialPort>,
}

impl PttController {
    /// Ouvre le port et applique tout de suite la polarité RX.
    pub fn open(cfg: PttConfig) -> Result<Self, String> {
        // Baudrate sans importance pour piloter RTS/DTR, mais le builder le
        // demande. Timeout court : on n'écrit pas de données.
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

/// Slot partagé entre `AppState` et le worker TX. `None` = PTT désactivée
/// pour la session (pas configuré, ou ouverture échouée).
pub type SharedPtt = Arc<Mutex<Option<PttController>>>;

/// Énumère les ports série visibles par l'OS. Renvoie une liste de noms
/// (`COM3`, `/dev/ttyUSB0`...).
pub fn list_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|v| v.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}

/// (Re)tente d'ouvrir le port à partir des settings courants. Renvoie un
/// message d'état lisible côté UI.
///
/// - `Ok(Some(msg))` : PTT active (msg = "PTT prête sur COM3" par ex.)
/// - `Ok(None)`      : PTT désactivée par configuration (case décochée…)
/// - `Err(msg)`      : ouverture échouée — `slot` est laissé à `None`.
pub fn refresh(slot: &SharedPtt, settings: &Settings) -> Result<Option<String>, String> {
    // On ferme l'ancien handle d'abord (drop = release du port côté OS).
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
