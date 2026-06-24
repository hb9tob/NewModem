//! Live LO / gain control for the Radio tab, via `sdrplay_api_Update`.
//!
//! Built from an open [`SdrplaySession`] and driven by the supervisor
//! thread (`rx::run_supervisor`) — the only non-callback context that may
//! safely call into the API while streaming. Implements the backend-
//! agnostic [`RadioHardware`] trait so the shared `RadioRuntime` applies
//! retune / gain commands without knowing this is an SDRplay.
//!
//! All raw pointers (`dev`, the active `rxChannel` params) are daemon-owned
//! and stay valid until `ReleaseDevice` (session `Drop`), which runs after
//! the supervisor loop exits — so a command never touches a freed pointer.

use modem_sdr::config::{GainSetting, ManualGainValue};
use modem_sdr_radio::RadioHardware;

use crate::api::{
    self, sdrplay_api_ReasonForUpdateExtension1T, sdrplay_api_ReasonForUpdateT,
    sdrplay_api_RxChannelParamsT, sdrplay_api_TunerSelectT, SdrplayApi, HANDLE,
};
use crate::device::{SdrplaySession, Tuner};

/// Live tuner control for the active SDRplay RX channel while streaming.
pub(crate) struct SdrplayRadioHw {
    dev: HANDLE,
    rx_chan: *mut sdrplay_api_RxChannelParamsT,
    tuner: sdrplay_api_TunerSelectT,
    lib: &'static SdrplayApi,
}

// SAFETY: same contract as `SdrplaySession` (device.rs) — the device
// handle and the params pointers are descriptors that move across threads
// as long as API calls are serialised. The supervisor thread is the only
// caller, so they are.
unsafe impl Send for SdrplayRadioHw {}

impl SdrplayRadioHw {
    /// Resolve the active rxChannel pointer (A or B) exactly the way
    /// `device::program_params` does at open.
    pub(crate) fn new(session: &SdrplaySession, lib: &'static SdrplayApi) -> Self {
        // SAFETY: `params` came from `GetDeviceParams`; the daemon keeps
        // the whole tree valid until `ReleaseDevice`.
        let rx_chan = unsafe {
            match session.config.tuner {
                Tuner::A => (*session.params).rxChannelA,
                Tuner::B => (*session.params).rxChannelB,
            }
        };
        Self {
            dev: session.device.dev,
            rx_chan,
            tuner: session.device.tuner,
            lib,
        }
    }

    fn update(&self, reason: sdrplay_api_ReasonForUpdateT, what: &str) {
        // SAFETY: `dev`/`tuner` came from the open session; `reason` is a
        // valid enum; the daemon serialises the call internally.
        let rc = unsafe {
            self.lib.sdrplay_api_Update(
                self.dev,
                self.tuner,
                reason,
                sdrplay_api_ReasonForUpdateExtension1T::sdrplay_api_Update_Ext1_None,
            )
        };
        if rc != api::sdrplay_api_ErrT::sdrplay_api_Success {
            eprintln!("[sdrplay-radio] Update {what} failed: code={}", rc as i32);
        }
    }
}

impl RadioHardware for SdrplayRadioHw {
    fn retune_lo(&mut self, lo_hz: u64) {
        if self.rx_chan.is_null() {
            return;
        }
        // `lo_hz` is already the programmed LO (the runtime carries the
        // +lo_offset), so write it straight to `rfHz` — mirrors the open
        // path (`device::program_params`) without adding the offset twice.
        // SAFETY: `rx_chan` is the daemon-owned active channel params.
        unsafe {
            (*self.rx_chan).tunerParams.rfFreq.rfHz = lo_hz as f64;
        }
        self.update(
            sdrplay_api_ReasonForUpdateT::sdrplay_api_Update_Tuner_Frf,
            "Frf",
        );
    }

    fn set_gain(&mut self, gain: &GainSetting) {
        if self.rx_chan.is_null() {
            return;
        }
        // SAFETY: `rx_chan` is the daemon-owned active channel params.
        let g = unsafe { &mut (*self.rx_chan).tunerParams.gain };
        match gain {
            GainSetting::Manual(ManualGainValue::LnaPlusIf { lna_state, if_grdb }) => {
                g.gRdB = *if_grdb;
                g.LNAstate = *lna_state;
            }
            GainSetting::Manual(ManualGainValue::Db { db }) => {
                // The generic Radio-tab slider hands a single dB value; map
                // it onto IF gain reduction (gRdB 20..59, higher = less
                // gain). LNA state stays as configured at open.
                g.gRdB = (59 - *db).clamp(20, 59);
            }
            GainSetting::AgcMode {
                lna_state: Some(s), ..
            } => {
                g.LNAstate = *s;
            }
            _ => return,
        }
        self.update(
            sdrplay_api_ReasonForUpdateT::sdrplay_api_Update_Tuner_Gr,
            "Gr",
        );
    }
}
