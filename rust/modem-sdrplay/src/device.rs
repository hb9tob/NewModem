//! Open + configure any SDRplay RX device (RSPduo, RSP1A, …) for receive.
//!
//! Mirrors `modem_pluto::device`'s split: a [`SdrplayConfig`] describes
//! the front-end knobs the GUI lets the user touch, [`open`] turns it
//! into a live [`SdrplaySession`] (device selected on the daemon side,
//! params programmed) ready for [`crate::rx::start`] to drape an I/Q
//! callback over via `sdrplay_api_Init`.
//!
//! Hardware variants — the daemon's `GetDevices` call returns a `hwVer`
//! byte we map to [`SdrplayHardware`]. [`open`] branches on it so:
//!  * **RSPduo** uses `rspDuoMode = Single_Tuner`, `rspDuoTunerParams`
//!    for bias-T / antenna port / FM/DAB notches.
//!  * **RSP1A** and **RSP1B** ignore `rspDuoMode`, always pick
//!    `Tuner::A`, use `rsp1aTunerParams.biasTEnable` and
//!    `devParams.rsp1aParams` for the FM / DAB notches; antenna
//!    port is fixed at 50 Ω SMA. RSP1B is wired to the same struct
//!    contract — it's an electrical refresh, not a new API surface.
//!  * **RSP1** (the original) has no bias-T / notches / antenna
//!    selector — only the common rxChannel programming runs.
//! Other variants (RSP2, RSPdx, RSPdxR2) surface as
//! [`SdrplayError::Api`] until they grow their own branch.

use std::os::raw::c_uint;
use std::ptr;

use crate::api::{
    check, check_open, cstr_field_to_string, sdrplay_api_AgcControlT, sdrplay_api_Bw_MHzT,
    sdrplay_api_Close, sdrplay_api_DeviceParamsT, sdrplay_api_DeviceT,
    sdrplay_api_GetDeviceParams, sdrplay_api_GetDevices, sdrplay_api_If_kHzT,
    sdrplay_api_LockDeviceApi, sdrplay_api_Open, sdrplay_api_ReleaseDevice,
    sdrplay_api_RspDuoModeT, sdrplay_api_RspDuo_AmPortSelectT, sdrplay_api_SelectDevice,
    sdrplay_api_TunerSelectT, sdrplay_api_UnlockDeviceApi,
};
use crate::error::SdrplayError;

/// SDRplay hardware family — derived from `sdrplay_api_DeviceT::hwVer`.
/// The daemon tells us which device is on the bus; we use that to
/// branch the params programming and to skip incompatible knobs (e.g.
/// `rspDuoMode` is meaningless on RSP1A).
///
/// `hwVer` constants come from SDRplay's `sdrplay_api.h`:
/// ```text
/// SDRPLAY_RSP1_ID    = 1
/// SDRPLAY_RSP2_ID    = 2
/// SDRPLAY_RSPduo_ID  = 3
/// SDRPLAY_RSPdx_ID   = 4
/// SDRPLAY_RSP1B_ID   = 6
/// SDRPLAY_RSPdxR2_ID = 7
/// SDRPLAY_RSP1A_ID   = 255
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdrplayHardware {
    Rsp1,
    Rsp1a,
    Rsp1b,
    Rsp2,
    RspDuo,
    RspDx,
    RspDxR2,
    /// Hardware byte the daemon returned but we don't program yet —
    /// `open()` rejects it with a clear error so the GUI shows the
    /// device but refuses to start streaming.
    Unsupported(u8),
}

impl SdrplayHardware {
    pub fn from_hw_ver(hw_ver: u8) -> Self {
        match hw_ver {
            1 => Self::Rsp1,
            2 => Self::Rsp2,
            3 => Self::RspDuo,
            4 => Self::RspDx,
            6 => Self::Rsp1b,
            7 => Self::RspDxR2,
            255 => Self::Rsp1a,
            v => Self::Unsupported(v),
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            Self::Rsp1 => "RSP1",
            Self::Rsp1a => "RSP1A",
            Self::Rsp1b => "RSP1B",
            Self::Rsp2 => "RSP2",
            Self::RspDuo => "RSPduo",
            Self::RspDx => "RSPdx",
            Self::RspDxR2 => "RSPdx-R2",
            Self::Unsupported(_) => "SDRplay",
        }
    }

    /// True when the device exposes a Tuner-A / Tuner-B selector
    /// (i.e. the GUI should surface the `tuner` extras dropdown).
    /// RSPduo is the only multi-tuner part in the family.
    pub fn has_tuner_selector(&self) -> bool {
        matches!(self, Self::RspDuo)
    }

    /// True when the device has a user-selectable antenna port. The
    /// RSPduo's tuner-A has Hi-Z vs 50 Ω; RSPdx has three. RSP1A
    /// only has a single SMA so the GUI hides the dropdown.
    pub fn has_antenna_selector(&self) -> bool {
        matches!(self, Self::RspDuo | Self::RspDx | Self::RspDxR2 | Self::Rsp2)
    }
}

/// Tuner half of an RSPduo. Single-tuner mode is enough for our 2 m
/// NBFM RX use case — diversity / dual-RX is a follow-up. On non-
/// RSPduo devices [`Tuner::A`] is the only meaningful value (the
/// daemon ignores Tuner-B selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tuner {
    /// Tuner 1. Has both the AM (Hi-Z) port and a 50 Ω port (RSPduo);
    /// the only tuner on RSP1 / RSP1A / RSP1B.
    A,
    /// Tuner 2. Single 50 Ω antenna. The bias-T runs on this port,
    /// so external preamps land here. RSPduo only.
    B,
}

/// Antenna port selection — only meaningful on Tuner A. Tuner B is
/// hard-wired to its single 50 Ω port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntennaPort {
    /// Tuner-1 high-impedance AM port (`AMPORT_1`). 1 kHz – 60 MHz —
    /// not useful at 145 MHz.
    Hiz,
    /// Tuner-1 50 Ω port (`AMPORT_2`). 60 MHz – 2 GHz. Default for
    /// any 144 / 430 MHz amateur work.
    Fifty,
}

/// IF gain AGC loop bandwidth. Maps onto `sdrplay_api_AgcControlT`
/// — when the AGC is enabled, the daemon adjusts `gRdB` automatically
/// to hold the IF level at `setPoint_dBfs` (default −60 dBFS); the
/// value the GUI surfaces in `if_gain_reduction_db` is then ignored.
/// LNA state is independent of the AGC and stays under operator
/// control whatever the AGC mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgcMode {
    /// AGC off — manual gain only. Use [`SdrplayConfig::if_gain_reduction_db`]
    /// to drive `gRdB` directly.
    Disable,
    /// 5 Hz loop bandwidth — slow tracking, immune to short bursts,
    /// useful when an HF preamp's level varies on-air ducting timescales.
    Slow,
    /// 50 Hz loop bandwidth — SDRplay's own default. Good general-
    /// purpose response on amateur VHF / UHF.
    Mid,
    /// 100 Hz loop bandwidth — fastest, follows fade quickly but can
    /// pump on strong adjacent-channel transients.
    Fast,
}

/// SDRplay device configuration the GUI / CLI pass into [`open`].
/// Maps onto a subset of `sdrplay_api_DevParamsT` +
/// `sdrplay_api_TunerParamsT` + the per-device sub-struct (RSPduo:
/// `RspDuoTunerParamsT`; RSP1A: `Rsp1aTunerParamsT` +
/// `Rsp1aParamsT`) — only the knobs we actually expose to the operator.
#[derive(Debug, Clone)]
pub struct SdrplayConfig {
    /// Device serial (e.g. `2401024C13` for RSPduo,
    /// `1500R76GR1` for RSP1A). Empty = first device the daemon
    /// hands back.
    pub serial: String,
    /// Which tuner half to bind in single-tuner mode. Ignored on
    /// devices without a tuner selector — `open()` overrides this
    /// to [`Tuner::A`] before programming.
    pub tuner: Tuner,
    /// Antenna port. Honoured only on the RSPduo (Tuner A's
    /// AMPORT_1 / AMPORT_2). Ignored on RSP1A — that part has a
    /// single SMA and the daemon doesn't model an antenna selector.
    pub antenna: AntennaPort,
    /// Bias-T on the active port. Wiring depends on hardware:
    /// RSPduo only exposes bias-T on Tuner B; RSP1A has bias-T on
    /// its single SMA port. The backend writes it onto whichever
    /// `*_TunerParamsT.biasTEnable` field the active hardware uses.
    /// Default false.
    pub bias_t: bool,
    /// Broadcast-FM rejection notch (~88 – 108 MHz). Default false.
    pub fm_notch: bool,
    /// DAB band-III rejection notch (~174 – 240 MHz). Default false.
    pub dab_notch: bool,
    /// RF tuner LO frequency in Hz. 145.5 MHz default (matches the
    /// modem-pluto default — 2 m simplex).
    pub rf_freq_hz: u64,
    /// Master / ADC sample rate in samples per second. The host I/Q
    /// rate is `sample_rate_hz / decimation`.
    pub sample_rate_hz: f64,
    /// API decimation factor (1, 2, 4, 8, 16, 32). Applied
    /// internally before the I/Q callback. With
    /// [`PREFERRED_SAMPLE_RATE_HZ`] (2.304 MS/s) and decim 4, the
    /// host receives 576 kSa/s — same rate as `modem_pluto` and the
    /// existing `modem_sdr_dsp` chain.
    pub decimation: u32,
    /// LNA state index — front-end attenuator setting. RSPduo VHF
    /// table has 10 states (0 = least attenuation = most gain). 4
    /// is a safe mid-band default.
    pub lna_state: u8,
    /// IF reduction in dB ("gain reduction"). Range typically
    /// 20 – 59. Default 40. Ignored when `agc_mode` is anything
    /// other than [`AgcMode::Disable`] — the daemon manages it.
    pub if_gain_reduction_db: i32,
    /// IF AGC loop bandwidth. [`AgcMode::Disable`] gives the
    /// operator full control of `if_gain_reduction_db`; the three
    /// active modes let the daemon track the on-air level and
    /// override `gRdB` at the chosen rate. LNA state stays manual
    /// in every mode. Default [`AgcMode::Disable`] so the GUI
    /// gain slider does what it says by default; flip to `Mid` for
    /// hands-off operation.
    pub agc_mode: AgcMode,
    /// Maximum FM deviation expected on air, in Hz. 5000 = NBFM
    /// standard. Drives the QuadratureDemod gain in the DSP chain.
    pub max_deviation_hz: f32,
}

impl Default for SdrplayConfig {
    fn default() -> Self {
        SdrplayConfig {
            serial: String::new(),
            // RSPduo-friendly default — Tuner B + 50 Ω. `open()`
            // overrides `tuner` to A when the daemon reports a non-
            // RSPduo device anyway, so single-tuner parts (RSP1A)
            // don't care what's set here.
            tuner: Tuner::B,
            antenna: AntennaPort::Fifty,
            bias_t: false,
            fm_notch: false,
            dab_notch: false,
            rf_freq_hz: 145_500_000,
            sample_rate_hz: PREFERRED_SAMPLE_RATE_HZ as f64,
            decimation: PREFERRED_DECIMATION,
            lna_state: 4,
            if_gain_reduction_db: 40,
            agc_mode: AgcMode::Disable,
            max_deviation_hz: 5_000.0,
        }
    }
}

/// Master sample rate for the RSPduo. **2_304_000 = 48 × 48000** —
/// a clean even multiple of the modem's 48 kHz audio rate with
/// small-prime composites only (factor 48 = 2⁴·3). With
/// [`PREFERRED_DECIMATION`] applied by the API, the host I/Q rate
/// is 576 kSa/s — ÷12 (also = 2²·3) to 48 kHz audio in
/// `modem-sdr-dsp`'s polyphase decimator. Matches `modem_pluto`'s
/// preferred rate exactly so the same DSP chain works unchanged.
pub const PREFERRED_SAMPLE_RATE_HZ: u64 = 2_304_000;
pub const PREFERRED_DECIMATION: u32 = 4;
/// Decimation ratio our DSP chain applies after the API hands us
/// samples at 576 kSa/s.
pub const PREFERRED_AUDIO_RATIO: usize = 12;


/// Live device handle — owns the daemon-side selection. Dropping
/// this releases the device through `sdrplay_api_ReleaseDevice` and
/// closes the API.
pub struct SdrplaySession {
    pub(crate) device: sdrplay_api_DeviceT,
    /// Pointer the API hands back from `GetDeviceParams`. Lives as
    /// long as the daemon-side selection — invalidated by
    /// `ReleaseDevice`. Kept here for future `sdrplay_api_Update`
    /// calls (live retune / gain change while streaming); RX path
    /// programs everything once at open.
    #[allow(dead_code)]
    pub(crate) params: *mut sdrplay_api_DeviceParamsT,
    /// Hardware family the daemon reported at `GetDevices` time.
    /// Captured here so a future `update_*` call programs into the
    /// right per-device sub-struct without re-querying.
    pub hardware: SdrplayHardware,
    /// Echo of the input config so [`crate::rx`] can rebuild the DSP
    /// chain (deviation, audio ratio).
    pub config: SdrplayConfig,
}

// SAFETY: the daemon owns the hardware. On the client side the
// device handle and params pointer are descriptors that can move
// across thread boundaries as long as we serialise calls into the
// API (which the daemon enforces internally with its own locking).
unsafe impl Send for SdrplaySession {}

impl Drop for SdrplaySession {
    fn drop(&mut self) {
        // Best-effort cleanup. We don't propagate errors — the
        // worker is shutting down anyway, and the daemon GCs the
        // selection if our process simply exits.
        // SAFETY: contract is honoured: no concurrent API calls
        // remain (the RX thread owns the only `Init` and joins
        // before its `SdrplaySession` drop), and the device handle
        // came from `SelectDevice` so `ReleaseDevice` is the
        // matching teardown.
        unsafe {
            let _ = sdrplay_api_ReleaseDevice(&mut self.device as *mut _);
            let _ = sdrplay_api_Close();
        }
    }
}

/// Enumerate every SDRplay device visible to the daemon. Returns
/// the serial-number list — the GUI surfaces them as
/// `sdrplay:<serial>`. Use [`list_devices_meta`] when you also need
/// the hardware family (for friendly names / capability gating).
pub fn list_serials() -> Result<Vec<String>, SdrplayError> {
    Ok(list_devices_meta()?
        .into_iter()
        .map(|(serial, _)| serial)
        .collect())
}

/// Enumerate visible devices with their `(serial, hardware)` pair.
/// The backend uses this to label devices ("SDRplay RSP1A — …" vs
/// "SDRplay RSPduo — …") in the GUI dropdown.
pub fn list_devices_meta() -> Result<Vec<(String, SdrplayHardware)>, SdrplayError> {
    // Delay-load guard (Windows): if sdrplay_api.dll isn't installed
    // at all, surface DllMissing rather than crash on the first API
    // call. No-op on Linux (ELF loader catches it at startup).
    crate::runtime_guard::ensure_dll_loadable()?;

    // SAFETY: `sdrplay_api_Open` is reference-counted on the daemon
    // side, the `[DeviceT; 8]` fits the API's documented max device
    // count, and `numDevs` is initialised to 0 before being passed
    // by pointer.
    unsafe { check_open(sdrplay_api_Open())? };

    let mut devices: [sdrplay_api_DeviceT; 8] = unsafe { std::mem::zeroed() };
    let mut n_devs: c_uint = 0;
    let res = unsafe {
        check(
            "GetDevices",
            sdrplay_api_GetDevices(
                devices.as_mut_ptr(),
                &mut n_devs as *mut _,
                devices.len() as c_uint,
            ),
        )
    };
    let _ = unsafe { sdrplay_api_Close() };
    res?;

    Ok(devices[..n_devs as usize]
        .iter()
        .map(|d| {
            (
                cstr_field_to_string(&d.SerNo),
                SdrplayHardware::from_hw_ver(d.hwVer),
            )
        })
        .collect())
}

/// Open the API, select the requested RSPduo, program the front-end
/// per [`SdrplayConfig`]. Streaming itself is started later by
/// [`crate::rx::start`] which calls `sdrplay_api_Init` with the
/// callback functions.
pub fn open(config: &SdrplayConfig) -> Result<SdrplaySession, SdrplayError> {
    // Delay-load guard mirrors `list_devices_meta`. Defensive: any
    // direct caller that skips enumeration still hits a typed error
    // rather than a delay-load SEH crash on Windows.
    crate::runtime_guard::ensure_dll_loadable()?;

    // Phase 1 — connect to the daemon and discover devices.
    // SAFETY: see `list_serials`. Same bounded fixed-size array
    // pattern, same daemon-owned lifecycle.
    unsafe { check_open(sdrplay_api_Open())? };

    let mut devices: [sdrplay_api_DeviceT; 8] = unsafe { std::mem::zeroed() };
    let mut n_devs: c_uint = 0;
    let getdevs = unsafe {
        check(
            "GetDevices",
            sdrplay_api_GetDevices(
                devices.as_mut_ptr(),
                &mut n_devs as *mut _,
                devices.len() as c_uint,
            ),
        )
    };
    if let Err(e) = getdevs {
        let _ = unsafe { sdrplay_api_Close() };
        return Err(e);
    }
    if n_devs == 0 {
        let _ = unsafe { sdrplay_api_Close() };
        return Err(SdrplayError::NoDevice);
    }

    // Phase 2 — pick the device matching `config.serial` (or the
    // first one when serial is empty).
    let pick_idx = if config.serial.is_empty() {
        0
    } else {
        match devices[..n_devs as usize]
            .iter()
            .position(|d| cstr_field_to_string(&d.SerNo) == config.serial)
        {
            Some(i) => i,
            None => {
                let _ = unsafe { sdrplay_api_Close() };
                return Err(SdrplayError::UnknownSerial(config.serial.clone()));
            }
        }
    };
    let mut device = devices[pick_idx];

    // Identify the hardware family from the byte the daemon set in
    // `hwVer`. Branches everything below: pre-Select fields on the
    // DeviceT, the per-tuner sub-struct in `program_params`, and
    // the GUI-side capability gating once `SdrplaySession.hardware`
    // bubbles back up.
    let hardware = SdrplayHardware::from_hw_ver(device.hwVer);
    if let SdrplayHardware::Unsupported(v) = hardware {
        let _ = unsafe { sdrplay_api_Close() };
        return Err(SdrplayError::Api {
            call: "GetDevices",
            code: -1,
            api_message: format!(
                "unsupported SDRplay hardware (hwVer={v}); \
                 RSPduo / RSP1A / RSP1B / RSP1 are wired"
            ),
        });
    }

    // Pre-SelectDevice fields on the DeviceT itself.
    //   * RSPduo → set `rspDuoMode = Single_Tuner` and forward the
    //     master sample rate via `rspDuoSampleFreq`. The daemon
    //     uses these to negotiate internal routing.
    //   * Other parts → leave `rspDuoMode` at `Unknown` (= 0); they
    //     have a single tuner, so we always map to Tuner-A even if
    //     the GUI-persisted config says B.
    let effective_tuner = if hardware.has_tuner_selector() {
        config.tuner
    } else {
        Tuner::A
    };
    if matches!(hardware, SdrplayHardware::RspDuo) {
        device.rspDuoMode = sdrplay_api_RspDuoModeT::sdrplay_api_RspDuoMode_Single_Tuner;
        device.rspDuoSampleFreq = config.sample_rate_hz;
    }
    device.tuner = match effective_tuner {
        Tuner::A => sdrplay_api_TunerSelectT::sdrplay_api_Tuner_A,
        Tuner::B => sdrplay_api_TunerSelectT::sdrplay_api_Tuner_B,
    };

    // Phase 3 — atomic select+program against the locked API.
    // SAFETY: Lock / Unlock pair guarantees the daemon serialises
    // our SelectDevice against any other client.
    if let Err(e) = unsafe { check("LockDeviceApi", sdrplay_api_LockDeviceApi()) } {
        let _ = unsafe { sdrplay_api_Close() };
        return Err(e);
    }
    let select_res = unsafe {
        check(
            "SelectDevice",
            sdrplay_api_SelectDevice(&mut device as *mut _),
        )
    };
    let _ = unsafe { sdrplay_api_UnlockDeviceApi() };
    if let Err(e) = select_res {
        let _ = unsafe { sdrplay_api_Close() };
        return Err(e);
    }

    // Phase 4 — pull the params pointer and program the front-end.
    let mut params: *mut sdrplay_api_DeviceParamsT = ptr::null_mut();
    let getparams = unsafe {
        check(
            "GetDeviceParams",
            sdrplay_api_GetDeviceParams(device.dev, &mut params as *mut _),
        )
    };
    if let Err(e) = getparams {
        let _ = unsafe { sdrplay_api_ReleaseDevice(&mut device as *mut _) };
        let _ = unsafe { sdrplay_api_Close() };
        return Err(e);
    }
    if params.is_null() {
        let _ = unsafe { sdrplay_api_ReleaseDevice(&mut device as *mut _) };
        let _ = unsafe { sdrplay_api_Close() };
        return Err(SdrplayError::Api {
            call: "GetDeviceParams",
            code: -1,
            api_message: "params pointer was null".into(),
        });
    }

    // Pass the resolved hardware + tuner down to params programming
    // so it knows whether to write to RspDuo / Rsp1a sub-structs.
    let mut effective_config = config.clone();
    effective_config.tuner = effective_tuner;
    if let Err(e) = program_params(params, &effective_config, hardware) {
        let _ = unsafe { sdrplay_api_ReleaseDevice(&mut device as *mut _) };
        let _ = unsafe { sdrplay_api_Close() };
        return Err(e);
    }

    Ok(SdrplaySession {
        device,
        params,
        hardware,
        config: effective_config,
    })
}

/// Apply [`SdrplayConfig`] onto the daemon's params tree. Called by
/// [`open`] before streaming starts; mutating these fields later
/// requires `sdrplay_api_Update` (a follow-up — for the modem use
/// case we only need to set everything once at open).
///
/// Common fields (`fsHz`, `rfHz`, gain table, bandwidth, decimation,
/// AGC) are programmed identically on every device. Bias-T, antenna
/// port and FM/DAB notches live on per-device sub-structs and
/// branch on `hardware`.
fn program_params(
    params: *mut sdrplay_api_DeviceParamsT,
    config: &SdrplayConfig,
    hardware: SdrplayHardware,
) -> Result<(), SdrplayError> {
    // SAFETY: `params` came directly out of `GetDeviceParams`; the
    // daemon guarantees the entire tree (devParams + per-tuner
    // rxChannelA / rxChannelB) is valid until `ReleaseDevice` runs.
    // We hold no aliased references to any sub-field.
    unsafe {
        // Clock + master sample rate (devParams shared by both tuners).
        let dev_params = (*params).devParams;
        if dev_params.is_null() {
            return Err(SdrplayError::Api {
                call: "GetDeviceParams",
                code: -1,
                api_message: "devParams pointer was null".into(),
            });
        }
        (*dev_params).fsFreq.fsHz = config.sample_rate_hz;

        // FM / DAB notch lives on the device-level sub-struct for
        // RSP1A and RSP1B (both share `rsp1aParams` — the RSP1B is
        // an electrical refresh of the RSP1A and reuses its API
        // contract). On RSPduo the same notches live on the
        // rxChannel sub-struct (handled below). RSP1 has no notch
        // hardware so there's nothing to write. Programmed here so
        // we touch `dev_params` exactly once.
        if matches!(
            hardware,
            SdrplayHardware::Rsp1a | SdrplayHardware::Rsp1b
        ) {
            let rsp1a = &mut (*dev_params).rsp1aParams;
            rsp1a.rfNotchEnable = if config.fm_notch { 1 } else { 0 };
            rsp1a.rfDabNotchEnable = if config.dab_notch { 1 } else { 0 };
        }

        // The active RX channel struct depends on which tuner the
        // daemon assigned. RSPduo's single-tuner-on-B path uses the
        // rxChannelB slot; everywhere else we always read rxChannelA.
        let rx_chan_ptr = match config.tuner {
            Tuner::A => (*params).rxChannelA,
            Tuner::B => (*params).rxChannelB,
        };
        if rx_chan_ptr.is_null() {
            return Err(SdrplayError::Api {
                call: "GetDeviceParams",
                code: -1,
                api_message: "active rxChannel pointer was null".into(),
            });
        }

        // Tuner-side params: RF LO, IF gain reduction, LNA state, BW.
        let tuner_params = &mut (*rx_chan_ptr).tunerParams;
        tuner_params.rfFreq.rfHz = config.rf_freq_hz as f64;
        tuner_params.gain.gRdB = config.if_gain_reduction_db;
        tuner_params.gain.LNAstate = config.lna_state;
        tuner_params.bwType = sdrplay_api_Bw_MHzT::sdrplay_api_BW_1_536;
        tuner_params.ifType = sdrplay_api_If_kHzT::sdrplay_api_IF_Zero;

        // Control-side params: API decimation + manual gain (AGC off).
        let ctrl = &mut (*rx_chan_ptr).ctrlParams;
        ctrl.decimation.enable = if config.decimation > 1 { 1 } else { 0 };
        ctrl.decimation.decimationFactor = config.decimation as u8;
        // Wide-band lowpass mode — keeps the IF filter centred for
        // even-decimation factors.
        ctrl.decimation.wideBandSignal = 1;
        ctrl.agc.enable = match config.agc_mode {
            AgcMode::Disable => sdrplay_api_AgcControlT::sdrplay_api_AGC_DISABLE,
            AgcMode::Slow => sdrplay_api_AgcControlT::sdrplay_api_AGC_5HZ,
            AgcMode::Mid => sdrplay_api_AgcControlT::sdrplay_api_AGC_50HZ,
            AgcMode::Fast => sdrplay_api_AgcControlT::sdrplay_api_AGC_100HZ,
        };

        // Bias-T + antenna port + FM/DAB notch — wiring depends on
        // hardware. Each branch writes only the fields it owns.
        match hardware {
            SdrplayHardware::RspDuo => {
                // cf. sdrplay_api_rspDuo.h
                let duo = &mut (*rx_chan_ptr).rspDuoTunerParams;
                duo.biasTEnable = if config.bias_t { 1 } else { 0 };
                duo.tuner1AmPortSel = match config.antenna {
                    AntennaPort::Hiz => {
                        sdrplay_api_RspDuo_AmPortSelectT::sdrplay_api_RspDuo_AMPORT_1
                    }
                    AntennaPort::Fifty => {
                        sdrplay_api_RspDuo_AmPortSelectT::sdrplay_api_RspDuo_AMPORT_2
                    }
                };
                duo.rfNotchEnable = if config.fm_notch { 1 } else { 0 };
                duo.rfDabNotchEnable = if config.dab_notch { 1 } else { 0 };
                // tuner1AmNotchEnable left at 0 — only affects the AM
                // (Hi-Z) port we don't use for VHF.
            }
            SdrplayHardware::Rsp1a | SdrplayHardware::Rsp1b => {
                // cf. sdrplay_api_rsp1a.h. Notches sat on `dev_params`
                // (above); only bias-T lives on the rxChannel struct.
                // RSP1B reuses the RSP1A struct contract, so the same
                // write covers both.
                // Antenna port has no GUI counterpart on either part —
                // the single SMA is hard-wired.
                let rsp1a = &mut (*rx_chan_ptr).rsp1aTunerParams;
                rsp1a.biasTEnable = if config.bias_t { 1 } else { 0 };
            }
            SdrplayHardware::Rsp1 => {
                // RSP1 (the original, hwVer=1) has no bias-T, no notch
                // filters, and a single fixed SMA antenna. The common
                // rxChannel programming above is enough — there is no
                // `rsp1Params` / `rsp1TunerParams` in the API, by
                // design. The GUI's bias-T / FM-notch / DAB-notch
                // toggles are silently ignored; addressing that needs
                // device-aware capabilities (follow-up).
            }
            other => {
                // `open()` already filters `Unsupported(_)` out, but
                // RSP2 / RSPdx / RSPdxR2 don't have their per-device
                // branches yet. Surface as a clear error rather than
                // silently skip the per-device writes.
                return Err(SdrplayError::Api {
                    call: "program_params",
                    code: -1,
                    api_message: format!(
                        "no per-device programming wired for {} yet",
                        other.short_name()
                    ),
                });
            }
        }
    }
    Ok(())
}

// Surface the API version helper so callers (CLI / GUI) can log it.
pub use crate::api::api_version;
