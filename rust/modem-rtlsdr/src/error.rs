//! Typed errors for the RTL-SDR backend. Mirrors the shape of
//! [`modem_sdrplay::error::SdrplayError`] so the worker / GUI surface
//! looks consistent across SDR backends.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RtlsdrError {
    /// `librtlsdr.so` couldn't be loaded at runtime — Linux: the
    /// package isn't installed; Windows: `rtlsdr.dll` isn't on PATH or
    /// next to the binary. The GUI surfaces this as an inline
    /// "Bibliothèque manquante" status next to the Paramètres
    /// checkbox, then `list_devices()` returns an empty Vec so the
    /// device dropdown stays clean.
    #[error(
        "librtlsdr non disponible — installer le paquet `librtlsdr0` \
         (Debian/Ubuntu: `sudo apt install librtlsdr0`) ou la build du \
         fork rtl-sdr-blog pour le support du dongle V4"
    )]
    DllMissing,

    /// `rtlsdr_open` failed. Typical causes: another process already
    /// has the device claimed (`rtl_tcp`, gqrx, …), missing udev rule,
    /// or USB permission denied.
    #[error("rtlsdr_open(index={index}) failed: code={code}")]
    Open { index: u32, code: i32 },

    /// `rtlsdr_get_device_count` returned 0. Dongle not on USB, udev
    /// rule missing (`/etc/udev/rules.d/rtl-sdr.rules`), kernel
    /// `dvb_usb_rtl28xxu` driver shadowing the device (needs
    /// blacklisting), or the process can't see the USB descriptor.
    #[error("no RTL-SDR device detected on the USB bus")]
    NoDevice,

    /// We searched by serial and didn't find a matching dongle.
    #[error("no RTL-SDR device with serial '{0}'")]
    UnknownSerial(String),

    /// Generic API call returned a non-zero status. Captures the call
    /// name for diagnostics — most librtlsdr functions return 0 on
    /// success and a negative errno on failure.
    #[error("librtlsdr call '{call}' failed: code={code}")]
    Api { call: &'static str, code: i32 },

    /// Sample-rate or front-end parameter rejected. Returned with the
    /// offending value so callers can fall back to a supported rate.
    #[error("rtlsdr parameter '{param}' rejected: {detail}")]
    BadParam {
        param: &'static str,
        detail: String,
    },

    /// USB read thread error — `rtlsdr_read_async` returned, the
    /// mpsc receiver hung up, or the supervisor saw too many
    /// consecutive frame errors.
    #[error("rtlsdr streaming: {0}")]
    Stream(String),
}
