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
use crate::device::{SdrplayHardware, SdrplaySession, Tuner};

/// Live tuner control for the active SDRplay RX channel while streaming.
pub(crate) struct SdrplayRadioHw {
    dev: HANDLE,
    rx_chan: *mut sdrplay_api_RxChannelParamsT,
    tuner: sdrplay_api_TunerSelectT,
    lib: &'static SdrplayApi,
    /// Device model — drives the frequency-aware LNA-state clamp. On the
    /// RSPdx family the number of valid `LNAstate` indices shrinks in the
    /// higher bands, so the max can't be cached as a single number the way
    /// the flat-table parts allow; it's recomputed from the current LO on
    /// every gain change / retune. An out-of-range `LNAstate` crashes the
    /// SDR service, so this clamp is mandatory.
    hardware: SdrplayHardware,
    /// HDR mode from the open config — only shifts the LNA count below
    /// 2 MHz, but carried so the clamp matches what `program_params` did.
    hdr_mode: bool,
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
            hardware: session.hardware,
            hdr_mode: session.config.hdr,
        }
    }

    /// Highest valid `LNAstate` index at the channel's current LO — the
    /// per-band clamp ceiling. Reads `rfFreq.rfHz` off the daemon-owned
    /// params tree (already carries the +LO offset; the sub-MHz offset is
    /// immaterial to the band boundaries).
    ///
    /// SAFETY: caller must ensure `rx_chan` is non-null.
    unsafe fn max_lna_state(&self) -> u8 {
        let rf_hz = (*self.rx_chan).tunerParams.rfFreq.rfHz as u64;
        self.hardware
            .lna_state_count_at(rf_hz, self.hdr_mode)
            .saturating_sub(1)
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
        // On the RSPdx family the number of valid LNA states shrinks in the
        // higher bands. Moving to a lower-state band while the current
        // LNAstate exceeds the new maximum makes the daemon abort. Lower the
        // gain FIRST (a reduction is valid in the old band too, since the new
        // ceiling is ≤ the old one), issue the Gr update, THEN retune — so
        // there is never an invalid intermediate state. On flat-table parts
        // (RSPduo / RSP1x) the ceiling never shrinks, so this is a no-op.
        // SAFETY: `rx_chan` is the daemon-owned active channel params.
        let new_max = self
            .hardware
            .lna_state_count_at(lo_hz, self.hdr_mode)
            .saturating_sub(1);
        let cur_lna = unsafe { (*self.rx_chan).tunerParams.gain.LNAstate };
        if cur_lna > new_max {
            unsafe {
                (*self.rx_chan).tunerParams.gain.LNAstate = new_max;
            }
            self.update(
                sdrplay_api_ReasonForUpdateT::sdrplay_api_Update_Tuner_Gr,
                "Gr(band-reclamp)",
            );
        }
        // `lo_hz` is already the programmed LO (the runtime carries the
        // +lo_offset), so write it straight to `rfHz` — mirrors the open
        // path (`device::program_params`) without adding the offset twice.
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
        // SAFETY: `rx_chan` is the daemon-owned active channel params
        // (non-null checked above); the LNA ceiling is band-aware for the
        // RSPdx family.
        let max_lna = unsafe { self.max_lna_state() };
        let g = unsafe { &mut (*self.rx_chan).tunerParams.gain };
        match gain {
            GainSetting::Manual(ManualGainValue::LnaPlusIf { lna_state, if_grdb }) => {
                g.gRdB = *if_grdb;
                // Clamp — an out-of-range index aborts the daemon (the GUI
                // input's `max` does not stop a typed value).
                g.LNAstate = (*lna_state).min(max_lna);
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
                g.LNAstate = (*s).min(max_lna);
            }
            _ => return,
        }
        self.update(
            sdrplay_api_ReasonForUpdateT::sdrplay_api_Update_Tuner_Gr,
            "Gr",
        );
    }
}
