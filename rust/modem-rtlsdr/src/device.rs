//! Open + configure an RTL-SDR dongle for receive.
//!
//! Mirrors `modem_sdrplay::device`'s split: a [`RtlsdrConfig`] carries
//! the front-end knobs the GUI exposes; [`open`] turns it into a live
//! [`RtlsdrSession`] (device claimed, programmed, ready for the RX
//! callback path in [`crate::rx`]).
//!
//! Hardware variants — both the RTL-SDR Blog **V3** (R820T2) and **V4**
//! (R828D) use the rtl-sdr-blog fork of librtlsdr. Their tuner gain
//! tables are identical (29 steps from 0.0 dB to 49.6 dB) and they
//! share the same VHF/UHF tuning range. The backend reads the live
//! USB strings + tuner type so the GUI surfaces the actual product
//! name; functional differences (V3's direct-sampling HF mode, V4's
//! upconverter) are not yet wired — see the `direct_sampling` extras
//! key in the crate-level docs for a future opt-in.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::Mutex;

use crate::error::RtlsdrError;
use crate::ffi::{
    rtlsdr_dev_t, RtlsdrLib, RTLSDR_TUNER_R820T, RTLSDR_TUNER_R828D,
};

/// R820T-family discrete tuner gain table, in **tenths of dB**, as
/// reported by `rtl_test` on the RTL-SDR Blog V4 and identical on the
/// V3. We hardcode it (rather than calling `rtlsdr_get_tuner_gains` at
/// every `capabilities()` query) so the GUI's
/// [`modem_sdr::BackendCapabilities`] can be a `static` cell.
///
/// Values are tenths of dB to dodge floats in the const expression;
/// divide by 10 for the dB ladder the GUI displays.
pub const GAIN_TABLE_TENTHS_DB: [i32; 29] = [
    0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280,
    297, 328, 338, 364, 372, 386, 402, 421, 434, 439, 445, 480, 496,
];

/// Master sample rate fed to the dongle, in Sa/s. **2_304_000 =
/// 48 × 48000** — clean even multiple of the 48 kHz audio rate with
/// small-prime composites only (factor 48 = 2⁴·3). The DSP chain
/// decimates ÷48 in [`modem_sdr_dsp::NbfmRxChain`]; tap budget lands in
/// the bracket already DSP-tested by
/// `nbfm_rx_chain::pluto_fallback_rate_works`.
pub const PREFERRED_SAMPLE_RATE_HZ: u32 = 2_304_000;

/// LO-offset compensation, in Hz. The R820T-family has a wider DC
/// artefact than the SDRplay Mirics MSi001 (LO leakage + amplitude
/// imbalance smear roughly ±100 kHz around DC), so we pull the LO
/// further away than the SDRplay's 75 kHz. **+250 kHz** keeps the
/// wanted channel comfortably off the spike while staying inside the
/// useful bandwidth of the 2.304 MS/s stream.
///
/// Sign convention: positive means LO is programmed *above* the
/// user's target frequency. The wanted signal then appears at
/// `−lo_offset_hz` in the IQ baseband, and `NbfmRxChain` (given
/// `lo_offset_hz = +250_000`) mixes it back up to DC.
pub const DEFAULT_LO_OFFSET_HZ: i32 = 250_000;

/// USB transfer buffer length, in bytes. librtlsdr's default is
/// 16 × 16 384 = 262 144 = 128 kSa I/Q ≈ 57 ms at 2.304 MS/s — too
/// laggy for a real-time NBFM listen. 16 384 = 8 kSa I/Q ≈ 3.55 ms.
/// Must be a multiple of 512 (USB block size); 16 384 satisfies it.
const READ_ASYNC_BUF_LEN: u32 = 16_384;

/// Number of concurrent USB transfers librtlsdr keeps in flight. 8 is
/// what `rtl_sdr` itself uses — enough headroom that a single slow
/// callback doesn't cause UNDERRUN on the dongle.
const READ_ASYNC_BUF_NUM: u32 = 8;

/// Hardware variant detected at enumeration time, used for friendly
/// labelling in the GUI dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtlsdrHardware {
    /// RTL-SDR Blog V4 (R828D tuner). Product string contains `"V4"`.
    BlogV4,
    /// RTL-SDR Blog V3 (R820T2 tuner). Product string contains `"V3"`
    /// or is the legacy `"RTL2838UHIDIR"` / `"Blog"` w/ R820T tuner.
    BlogV3,
    /// Anything else that opens — generic RTL2832U + R820T/R828D dongle.
    Generic,
}

impl RtlsdrHardware {
    pub fn short_name(&self) -> &'static str {
        match self {
            Self::BlogV4 => "RTL-SDR Blog V4",
            Self::BlogV3 => "RTL-SDR Blog V3",
            Self::Generic => "RTL-SDR",
        }
    }

    /// `DeviceDescriptor::hardware_hint` stamp. Lowercase snake-case per
    /// the convention documented in `modem_sdr::DeviceDescriptor`.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::BlogV4 => "blog_v4",
            Self::BlogV3 => "blog_v3",
            Self::Generic => "generic",
        }
    }
}

fn classify(product: &str, tuner_type: c_int) -> RtlsdrHardware {
    let p = product.to_ascii_lowercase();
    if p.contains("blog v4") || p.contains("blogv4") || p.contains("v4") {
        // V4 product string is "Blog V4"; R828D tuner.
        if tuner_type == RTLSDR_TUNER_R828D {
            return RtlsdrHardware::BlogV4;
        }
    }
    if p.contains("blog v3") || p.contains("blogv3") {
        return RtlsdrHardware::BlogV3;
    }
    // R820T2 (V3 or older Blog dongles) + non-V4-product → V3
    if tuner_type == RTLSDR_TUNER_R820T && p.contains("blog") {
        return RtlsdrHardware::BlogV3;
    }
    RtlsdrHardware::Generic
}

/// Generic RTL-SDR configuration the GUI / CLI pass into [`open`].
#[derive(Debug, Clone)]
pub struct RtlsdrConfig {
    /// Device serial (e.g. `"00000001"`). Empty = first device the
    /// librtlsdr enumeration returns.
    pub serial: String,
    /// RF tuner LO frequency in Hz. 145.5 MHz default (2 m simplex —
    /// matches the modem-sdrplay default).
    pub rf_freq_hz: u64,
    /// Sample rate handed to `rtlsdr_set_sample_rate`. Defaults to
    /// [`PREFERRED_SAMPLE_RATE_HZ`].
    pub sample_rate_hz: u32,
    /// Step index into [`GAIN_TABLE_TENTHS_DB`]. Defaults to the
    /// mid-table entry (≈ 25 dB). Ignored when `tuner_agc_enabled =
    /// true`.
    pub gain_step_idx: usize,
    /// `true` ⇒ tuner AGC drives the IF VGA, the `gain_step_idx` value
    /// is still pre-programmed as a fallback. Defaults to `false` —
    /// the operator's slider does what it says by default.
    pub tuner_agc_enabled: bool,
    /// `true` ⇒ RTL2832U digital AGC enabled. Independent of tuner
    /// AGC. Most operators leave this off.
    pub rtl_agc_enabled: bool,
    /// Bias-T on the antenna port. RTL-SDR Blog V3 and V4 both expose
    /// this on a GPIO pin (the API call is the same).
    pub bias_t: bool,
    /// Crystal-error correction in ppm
    /// (`rtlsdr_set_freq_correction`). Defaults to 0.
    pub ppm_correction: i32,
    /// V3-only HF mode. `false` everywhere else.
    pub direct_sampling: bool,
    /// Optional analog tuner bandwidth — `0` = pick the tuner's
    /// default (≈ 2 MHz for R820T-family).
    pub tuner_bandwidth_hz: u32,
    /// Maximum FM deviation expected on air, in Hz. 5 000 = NBFM
    /// standard. Drives the QuadratureDemod gain in the DSP chain.
    pub max_deviation_hz: f32,
}

impl Default for RtlsdrConfig {
    fn default() -> Self {
        RtlsdrConfig {
            serial: String::new(),
            rf_freq_hz: 145_500_000,
            sample_rate_hz: PREFERRED_SAMPLE_RATE_HZ,
            // Mid-table entry — ≈ 28 dB on the R820T-family table.
            gain_step_idx: 15,
            tuner_agc_enabled: false,
            rtl_agc_enabled: false,
            bias_t: false,
            ppm_correction: 0,
            direct_sampling: false,
            tuner_bandwidth_hz: 0,
            max_deviation_hz: 5_000.0,
        }
    }
}

/// Live dongle handle. Wraps the `*mut rtlsdr_dev_t` returned by
/// `rtlsdr_open` and closes it on Drop. The `Mutex` is a defensive
/// no-op for the happy path (the RX path takes the session by value
/// to spawn the supervisor) but protects against accidental concurrent
/// API calls if a future caller decides to share a session.
pub struct RtlsdrSession {
    /// Some(...) while owned by us; None after the RX thread has
    /// taken it via `take_dev`.
    dev: Mutex<Option<*mut rtlsdr_dev_t>>,
    pub hardware: RtlsdrHardware,
    pub config: RtlsdrConfig,
}

// SAFETY: librtlsdr serialises USB I/O internally per device; the
// raw pointer is Send because we hold the only reference to it (the
// Mutex makes it impossible to access from two threads at once).
unsafe impl Send for RtlsdrSession {}

impl RtlsdrSession {
    /// Hand the raw device pointer off to the RX worker thread. Sets
    /// the inner slot to `None` so this session's Drop becomes a
    /// no-op — the RX worker owns teardown from there on.
    pub(crate) fn take_dev(&self) -> Option<*mut rtlsdr_dev_t> {
        self.dev.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for RtlsdrSession {
    fn drop(&mut self) {
        if let Ok(mut g) = self.dev.lock() {
            if let Some(dev) = g.take() {
                if let Ok(lib) = RtlsdrLib::get() {
                    // SAFETY: dev came from rtlsdr_open and was never
                    // handed to another owner.
                    unsafe {
                        let _ = (lib.close)(dev);
                    }
                }
            }
        }
    }
}

/// Enumerate every RTL-SDR dongle visible to the system. Returns
/// `Ok(Vec::new())` when librtlsdr isn't loadable (the GUI surfaces
/// that case via the separate `library_available()` accessor — see
/// [`crate::backend::RtlsdrBackend`]).
pub fn list_devices_meta() -> Result<Vec<(String, String, RtlsdrHardware)>, RtlsdrError> {
    // Don't promote DllMissing into an error — return an empty list
    // and let the GUI's status indicator do its job.
    let lib = match RtlsdrLib::get() {
        Ok(l) => l,
        Err(RtlsdrError::DllMissing) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    // SAFETY: get_device_count has no preconditions.
    let n = unsafe { (lib.get_device_count)() };
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mut manufact = [0u8; 256];
        let mut product = [0u8; 256];
        let mut serial = [0u8; 256];
        // SAFETY: 256-byte buffers per the rtl-sdr.h doc-comment
        // ("the string arguments must provide space for up to 256
        // bytes").
        let rc = unsafe {
            (lib.get_device_usb_strings)(
                i,
                manufact.as_mut_ptr() as *mut c_char,
                product.as_mut_ptr() as *mut c_char,
                serial.as_mut_ptr() as *mut c_char,
            )
        };
        if rc != 0 {
            // Could not read strings for this index — surface the
            // dongle anyway with a synthetic name so the user can at
            // least open it by index. Hardware unknown.
            out.push((
                format!("rtlsdr_{i}"),
                format!("RTL-SDR (index {i})"),
                RtlsdrHardware::Generic,
            ));
            continue;
        }
        let product_s = cstr_buf_to_string(&product);
        let serial_s = cstr_buf_to_string(&serial);

        // Tuner type is only available after open() — but we don't
        // want to open every device just for the friendly name. Use
        // the product string alone to classify; classify() falls
        // through to Generic if no V3/V4 marker is present, which is
        // safe (just the label).
        let hw = classify(&product_s, /* tuner_type */ 0);
        let id = if serial_s.is_empty() {
            format!("idx:{i}")
        } else {
            serial_s
        };
        let friendly = if product_s.is_empty() {
            format!("{} — {id}", hw.short_name())
        } else {
            format!("{product_s} ({id})")
        };
        out.push((id, friendly, hw));
    }
    Ok(out)
}

/// Open + program the dongle described by `cfg`. Returns the live
/// session ready for [`crate::rx::start_on`].
pub fn open(cfg: &RtlsdrConfig) -> Result<RtlsdrSession, RtlsdrError> {
    let lib = RtlsdrLib::get()?;

    // SAFETY: get_device_count has no preconditions.
    let n = unsafe { (lib.get_device_count)() };
    if n == 0 {
        return Err(RtlsdrError::NoDevice);
    }

    // Resolve the requested serial to a device index. Empty serial =
    // pick the first dongle. Synthetic `"idx:N"` IDs from
    // list_devices_meta are honoured too.
    let pick_idx = if cfg.serial.is_empty() {
        0
    } else if let Some(rest) = cfg.serial.strip_prefix("idx:") {
        rest.parse::<u32>().unwrap_or(0)
    } else {
        let mut found: Option<u32> = None;
        for i in 0..n {
            let mut buf = [0u8; 256];
            // SAFETY: 256-byte buffer per librtlsdr docs.
            let rc = unsafe {
                (lib.get_device_usb_strings)(
                    i,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    buf.as_mut_ptr() as *mut c_char,
                )
            };
            if rc == 0 && cstr_buf_to_string(&buf) == cfg.serial {
                found = Some(i);
                break;
            }
        }
        match found {
            Some(i) => i,
            None => return Err(RtlsdrError::UnknownSerial(cfg.serial.clone())),
        }
    };

    // Open it. The blocking USB claim happens here.
    let mut dev: *mut rtlsdr_dev_t = ptr::null_mut();
    // SAFETY: dev points to a stack local. librtlsdr writes the handle
    // into *dev on success.
    let rc = unsafe { (lib.open)(&mut dev as *mut _, pick_idx) };
    if rc != 0 || dev.is_null() {
        return Err(RtlsdrError::Open {
            index: pick_idx,
            code: rc,
        });
    }

    // From here on, ANY error must close `dev` before returning.
    // Wrap programming in a closure so we can `?` freely without
    // leaking the handle.
    let prog = || -> Result<RtlsdrHardware, RtlsdrError> {
        // Identify hardware now that we have a handle.
        // SAFETY: dev is valid (just opened).
        let tuner_type = unsafe { (lib.get_tuner_type)(dev) };
        let mut product_buf = [0u8; 256];
        // SAFETY: 256-byte buffer.
        let _ = unsafe {
            (lib.get_device_usb_strings)(
                pick_idx,
                ptr::null_mut(),
                product_buf.as_mut_ptr() as *mut c_char,
                ptr::null_mut(),
            )
        };
        let hw = classify(&cstr_buf_to_string(&product_buf), tuner_type);

        // Direct sampling first — the rest of the front-end can vary
        // when this is on (Q-branch routing on V3).
        // SAFETY: dev valid; the call accepts 0|1|2 with anything
        // else clamped librtlsdr-side.
        check(
            "rtlsdr_set_direct_sampling",
            unsafe {
                (lib.set_direct_sampling)(dev, if cfg.direct_sampling { 2 } else { 0 })
            },
        )?;

        // Sample rate. librtlsdr accepts 225001–300000 or 900001–
        // 3200000; out-of-range is rejected with -EINVAL.
        check(
            "rtlsdr_set_sample_rate",
            unsafe { (lib.set_sample_rate)(dev, cfg.sample_rate_hz) },
        )?;

        // Tuner-bandwidth override (0 lets the driver pick the
        // default; the librtlsdr blog fork accepts 0 explicitly).
        check(
            "rtlsdr_set_tuner_bandwidth",
            unsafe { (lib.set_tuner_bandwidth)(dev, cfg.tuner_bandwidth_hz) },
        )?;

        // Frequency: program the LO above the user's target by
        // DEFAULT_LO_OFFSET_HZ so the wanted carrier sits off the DC
        // spike. NbfmRxChain's NCO will translate it back.
        let lo_hz = cfg
            .rf_freq_hz
            .saturating_add(DEFAULT_LO_OFFSET_HZ as u64);
        if lo_hz > u32::MAX as u64 {
            return Err(RtlsdrError::BadParam {
                param: "rf_freq_hz",
                detail: format!("requested LO {} Hz overflows u32", lo_hz),
            });
        }
        check(
            "rtlsdr_set_center_freq",
            unsafe { (lib.set_center_freq)(dev, lo_hz as u32) },
        )?;

        // PPM correction. librtlsdr returns -2 when the new ppm value
        // equals the currently-programmed one (e.g. 0 == 0 on a fresh
        // dongle) — that's a benign no-op the way librtlsdr signals
        // it, not an error. Treat -2 as success and only check for
        // truly negative codes.
        // SAFETY: dev is valid.
        let rc = unsafe { (lib.set_freq_correction)(dev, cfg.ppm_correction) };
        if rc != 0 && rc != -2 {
            return Err(RtlsdrError::Api {
                call: "rtlsdr_set_freq_correction",
                code: rc,
            });
        }

        // Gain — manual (0) vs tuner-AGC (1). librtlsdr's API has the
        // sense inverted from what's intuitive: `manual = 1` means the
        // operator drives the IF VGA, `manual = 0` means the tuner's
        // AGC loop does.
        let manual_flag = if cfg.tuner_agc_enabled { 0 } else { 1 };
        check(
            "rtlsdr_set_tuner_gain_mode",
            unsafe { (lib.set_tuner_gain_mode)(dev, manual_flag) },
        )?;
        // Always program the gain value, even in AGC mode — it acts as
        // the fallback the tuner uses when the AGC isn't actively
        // adjusting yet.
        let step = cfg
            .gain_step_idx
            .min(GAIN_TABLE_TENTHS_DB.len() - 1);
        check(
            "rtlsdr_set_tuner_gain",
            unsafe { (lib.set_tuner_gain)(dev, GAIN_TABLE_TENTHS_DB[step]) },
        )?;

        // RTL2832U digital AGC. Independent of tuner AGC.
        check(
            "rtlsdr_set_agc_mode",
            unsafe { (lib.set_agc_mode)(dev, if cfg.rtl_agc_enabled { 1 } else { 0 }) },
        )?;

        // Bias-T on the antenna port. The blog fork's
        // `rtlsdr_set_bias_tee` is a no-op on dongles without a
        // bias-T GPIO — safe to call unconditionally.
        check(
            "rtlsdr_set_bias_tee",
            unsafe { (lib.set_bias_tee)(dev, if cfg.bias_t { 1 } else { 0 }) },
        )?;

        // Required before the first read — flushes the FX2-side USB
        // buffer that may carry stale samples from a previous client.
        check(
            "rtlsdr_reset_buffer",
            unsafe { (lib.reset_buffer)(dev) },
        )?;

        Ok(hw)
    };

    let hardware = match prog() {
        Ok(hw) => hw,
        Err(e) => {
            // SAFETY: dev is valid; close before returning to release
            // the USB claim so a retry has a chance.
            unsafe {
                let _ = (lib.close)(dev);
            }
            return Err(e);
        }
    };

    Ok(RtlsdrSession {
        dev: Mutex::new(Some(dev)),
        hardware,
        config: cfg.clone(),
    })
}

/// Async-read transfer constants pulled out so [`crate::rx`] can reuse
/// the same defaults `rtl_sdr` /  `rtl_test` use.
pub(crate) const READ_BUF_LEN: u32 = READ_ASYNC_BUF_LEN;
pub(crate) const READ_BUF_NUM: u32 = READ_ASYNC_BUF_NUM;

/// Map a librtlsdr return code to `Result<(), RtlsdrError>`. Most
/// API calls return 0 on success and a negative errno on failure;
/// we don't try to interpret the errno beyond surfacing it.
fn check(call: &'static str, code: c_int) -> Result<(), RtlsdrError> {
    if code == 0 {
        Ok(())
    } else {
        Err(RtlsdrError::Api { call, code })
    }
}

/// Decode a `char[256]` buffer that librtlsdr populated with a
/// NUL-terminated ASCII string. Stops at the first 0 byte; replaces
/// invalid UTF-8 with the lossy substitute char.
fn cstr_buf_to_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    CStr::from_bytes_with_nul(if end < buf.len() {
        &buf[..=end]
    } else {
        // No NUL — synthesize one so CStr accepts the slice.
        return String::from_utf8_lossy(&buf[..end]).into_owned();
    })
    .map(|c| c.to_string_lossy().into_owned())
    .unwrap_or_default()
}

// Silence the "unused" warning while we wait for the eventual
// `update_*` retune surface to consume CString.
#[allow(dead_code)]
fn _force_cstring_link() -> CString {
    CString::new("placeholder").expect("static literal contains no NUL")
}
