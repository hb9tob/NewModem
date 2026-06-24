//! Live LO / gain control for the Radio tab, via librtlsdr.
//!
//! Built in [`crate::rx::start_on`] and driven by the dedicated control
//! thread — **never** the librtlsdr USB callback thread, where re-entering
//! a libusb control transfer would risk a deadlock. Implements the backend-
//! agnostic [`RadioHardware`] trait so the shared `RadioRuntime` applies
//! retune / gain commands without knowing this is an RTL-SDR.
//!
//! The device pointer is carried as a `usize` (Send) and used only for
//! `set_center_freq` / `set_tuner_gain*` — never `rtlsdr_close`, which the
//! capture thread owns. So there is exactly one close and no double-free.

use modem_sdr::config::{GainSetting, ManualGainValue};
use modem_sdr_radio::RadioHardware;

use crate::device::GAIN_TABLE_TENTHS_DB;
use crate::ffi::{rtlsdr_dev_t, RtlsdrLib};

/// Live tuner control for an RTL-SDR capture.
pub(crate) struct RtlHardware {
    /// Device pointer as `usize` so it crosses the thread boundary (raw
    /// pointers aren't `Send`). Same trick the capture thread uses.
    dev_addr: usize,
    lib: &'static RtlsdrLib,
}

// SAFETY: `dev_addr` is only ever passed back to librtlsdr's thread-safe
// tuner calls (set_center_freq / set_tuner_gain*), which are documented to
// be callable from a thread other than the one running read_async.
unsafe impl Send for RtlHardware {}

impl RtlHardware {
    pub(crate) fn new(dev_addr: usize, lib: &'static RtlsdrLib) -> Self {
        Self { dev_addr, lib }
    }

    fn dev(&self) -> *mut rtlsdr_dev_t {
        self.dev_addr as *mut rtlsdr_dev_t
    }
}

/// Nearest gain-table step (index into [`GAIN_TABLE_TENTHS_DB`]) for a dB
/// request from the generic Radio-tab slider.
fn nearest_gain_step(db: i32) -> usize {
    let target = db.saturating_mul(10); // table is tenths of a dB
    GAIN_TABLE_TENTHS_DB
        .iter()
        .enumerate()
        .min_by_key(|(_, &g)| (g - target).abs())
        .map(|(i, _)| i)
        .unwrap_or(GAIN_TABLE_TENTHS_DB.len() / 2)
}

impl RadioHardware for RtlHardware {
    fn retune_lo(&mut self, lo_hz: u64) {
        // `lo_hz` is already the programmed LO (user + lo_offset), so write
        // it straight — mirrors the open path (`device::open`).
        let dev = self.dev();
        // SAFETY: dev is the live device pointer; set_center_freq is a USB
        // control transfer librtlsdr allows from this (non-callback) thread.
        unsafe {
            let _ = (self.lib.set_center_freq)(dev, lo_hz.min(u32::MAX as u64) as u32);
        }
    }

    fn set_gain(&mut self, gain: &GainSetting) {
        let dev = self.dev();
        // librtlsdr sense is inverted: manual gain mode = 1, tuner AGC = 0.
        match gain {
            GainSetting::Manual(ManualGainValue::Db { db }) => {
                let tenths = GAIN_TABLE_TENTHS_DB[nearest_gain_step(*db)];
                // SAFETY: dev is live; manual gain + value, off the callback.
                unsafe {
                    let _ = (self.lib.set_tuner_gain_mode)(dev, 1);
                    let _ = (self.lib.set_tuner_gain)(dev, tenths);
                }
            }
            GainSetting::Manual(ManualGainValue::Discrete { step_idx }) => {
                let step = (*step_idx).min(GAIN_TABLE_TENTHS_DB.len() - 1);
                // SAFETY: see above.
                unsafe {
                    let _ = (self.lib.set_tuner_gain_mode)(dev, 1);
                    let _ = (self.lib.set_tuner_gain)(dev, GAIN_TABLE_TENTHS_DB[step]);
                }
            }
            GainSetting::AgcMode { .. } => {
                // SAFETY: hand the IF VGA to the tuner's own AGC loop.
                unsafe {
                    let _ = (self.lib.set_tuner_gain_mode)(dev, 0);
                }
            }
            _ => {}
        }
    }
}
