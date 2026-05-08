//! Backend-agnostic descriptor for a single physical SDR instance.
//!
//! Built by `SdrBackend::list_devices()` and round-trippable to
//! `SdrBackend::open()`. The `composite_name` field is the
//! GUI-facing handle that replaces the legacy magic-prefix scheme
//! (`pluto:usb:1.6.5`, `sdrplay:22340A2A34`) — same string format,
//! but now produced by the backend rather than concatenated in
//! `modem-gui/src-tauri/src/main.rs`.

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct DeviceDescriptor {
    /// Stable identifier of the producing backend (`SdrBackend::id`).
    /// Round-tripped to `sdr_registry::backend_by_id` to re-open this
    /// device.
    pub backend_id: &'static str,

    /// Backend-specific opaque ID — libiio URI for Pluto
    /// (`"usb:1.6.5"`, `"ip:pluto.local"`), serial number for
    /// SDRplay (`"22340A2A34"`). Whatever string the backend's
    /// `open()` accepts.
    pub id: String,

    /// Human-friendly label for the GUI dropdown row, e.g.
    /// `"PlutoSDR — usb:1.6.5"` or `"RSPduo (22340A2A34)"`.
    pub friendly_name: String,

    /// `"<backend_id>:<id>"`. The Tauri command `start_capture`
    /// receives this string and the registry's
    /// `parse_composite_name` splits it back into (backend, id)
    /// without any prefix-matching code in main.rs.
    pub composite_name: String,

    /// Backend-defined hardware family identifier. Stamped at
    /// `list_devices()` time so the GUI can query device-aware
    /// capabilities via [`crate::SdrBackend::capabilities_for`]
    /// without having to open the device first. Examples:
    ///
    /// - SDRplay: `"rspduo" | "rsp1a" | "rsp1b" | "rsp1"`
    /// - Pluto / RTL-SDR / single-flavour backends: `None`
    ///
    /// Backends are free to choose any string — the only consumer
    /// is the same backend's `capabilities_for` lookup. Lowercase
    /// snake-case is the convention for new backends.
    #[serde(default)]
    pub hardware_hint: Option<String>,
}

impl DeviceDescriptor {
    /// Convenience constructor that builds `composite_name` from the
    /// backend ID and device ID, in the canonical `"<backend>:<id>"`
    /// format. Backends should use this rather than hand-formatting
    /// the string to keep parsing/formatting in lock-step.
    pub fn new(backend_id: &'static str, id: impl Into<String>, friendly_name: impl Into<String>) -> Self {
        let id = id.into();
        let composite_name = format!("{backend_id}:{id}");
        Self {
            backend_id,
            id,
            friendly_name: friendly_name.into(),
            composite_name,
            hardware_hint: None,
        }
    }

    /// Builder that stamps the hardware family hint. Only meaningful
    /// for backends with multiple hardware variants (SDRplay today).
    pub fn with_hardware_hint(mut self, hint: impl Into<String>) -> Self {
        self.hardware_hint = Some(hint.into());
        self
    }
}
