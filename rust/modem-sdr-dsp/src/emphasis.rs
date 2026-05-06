//! Pre-emphasis (TX) and de-emphasis (RX) filters for NBFM.
//!
//! These match the 75 µs single-pole emphasis curve used by GNU
//! Radio's `analog::fm_preemph` / `analog::fm_deemph` and by typical
//! amateur-radio NBFM transceivers.
//!
//! The implementations already exist in the modem worker:
//! - `modem_worker::tx_worker::preemphasis_nbfm_48k` (lines 443-460)
//! - `modem_worker::rx_worker::DeemphasisFilter`     (lines 271-305)
//!
//! TODO(impl): move them here verbatim and leave thin re-exports in
//! `modem-worker` so existing call sites compile unchanged.
