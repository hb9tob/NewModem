//! Back-compat shim: the real PTT controller lives in
//! [`modem_worker_base::ptt`] now. Re-exports keep existing
//! `modem_worker::ptt::{PttController, PttConfig, SharedPtt, PTT_GUARD_MS, list_ports}`
//! call sites unchanged.

pub use modem_worker_base::ptt::{
    list_ports, PttConfig, PttController, SharedPtt, PTT_GUARD_MS,
};
