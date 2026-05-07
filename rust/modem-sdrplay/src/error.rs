//! Typed errors for the SDRplay backend. Mirrors the shape of
//! [`modem_pluto::error::PlutoError`] so the worker / GUI surface
//! looks consistent across SDR backends.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SdrplayError {
    /// `sdrplay_api_Open` failed — typical causes: daemon not running
    /// (`sudo systemctl start sdrplay`), API/library mismatch, the
    /// process can't reach the local socket the daemon listens on.
    #[error("sdrplay_api_Open failed: code={code} ({api_message})")]
    Open { code: i32, api_message: String },

    /// `sdrplay_api_GetDevices` returned 0 devices. RSPduo not on USB,
    /// udev rule missing (`/etc/udev/rules.d/66-sdrplay.rules`), or
    /// the daemon is running as a user that can't see the USB device.
    #[error("no SDRplay device detected on the bus")]
    NoDevice,

    /// We searched for a specific device by serial and didn't find it.
    #[error("no SDRplay device with serial '{0}' (try Settings → device dropdown)")]
    UnknownSerial(String),

    /// `sdrplay_api_SelectDevice` failed.
    #[error("sdrplay_api_SelectDevice failed: code={code} ({api_message})")]
    Select { code: i32, api_message: String },

    /// Any other API call returned a non-Success status. Captures the
    /// human-readable name from `sdrplay_api_GetErrorString` so the
    /// GUI can surface something better than a number.
    #[error("sdrplay_api call '{call}' failed: code={code} ({api_message})")]
    Api {
        call: &'static str,
        code: i32,
        api_message: String,
    },

    /// Sample-rate or front-end parameter rejected by the API as
    /// out-of-range. Returned with the offending value so callers can
    /// fall back (e.g. retry at the next standard rate).
    #[error("sdrplay parameter '{param}' rejected: {detail}")]
    BadParam {
        param: &'static str,
        detail: String,
    },

    /// I/Q stream callback or post-init configuration failure (e.g.
    /// the streaming thread couldn't push to the mpsc).
    #[error("sdrplay streaming: {0}")]
    Stream(String),
}
