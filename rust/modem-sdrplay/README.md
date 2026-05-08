# modem-sdrplay — SDRplay backend (RX-only)

Wraps the closed-binary [SDRplay API 3.x][sdrplay-api] into the same
seam the Pluto and cpal backends use:

```rust
let (handle, samples) = modem_sdrplay::rx::start(&config)?;
//                          ^^^^^^^^^^^^^^^^^^^^^^^^^
// Returns (CaptureHandle, mpsc::Receiver<Vec<f32>>) at 48 kHz mono —
// identical shape to modem_io::cpal_capture::start and
// modem_pluto::rx::start, so rx_worker is agnostic to which radio
// fed it.
```

[sdrplay-api]: https://www.sdrplay.com/api/

TX is out of scope — the whole RSP family is RX-only hardware.

## Supported devices

The daemon's `GetDevices` call returns a `hwVer` byte we map to
[`SdrplayHardware`]; `device::open` branches on it so each part talks
to the right per-device sub-struct in the API.

| Device | hwVer | Status | Notes |
|---|---|---|---|
| RSPduo | 3 | ✅ wired | Tuner A / B, AMPORT 1/2, bias-T, FM/DAB notch |
| RSP1A | 255 | ✅ wired | Single SMA, bias-T, FM/DAB notch, 10-state VHF LNA |
| RSP1B | 6 | ✅ wired | RSP1A's API contract verbatim (electrical refresh) |
| RSP1 | 1 | ✅ wired | Single SMA, **no** bias-T / notches, 4-state VHF LNA |
| RSP2 | 2 | ❌ scaffold only | Rejected with a clear error at `open()` |
| RSPdx | 4 | ❌ scaffold only | " |
| RSPdx-R2 | 7 | ❌ scaffold only | " |

`SdrBackend::capabilities_for(&descriptor)` returns the per-device
capability surface (different antenna list, gain table, feature set)
so the GUI panel only shows knobs the part can actually drive — no
bogus bias-T checkbox on the RSP1, no Hi-Z dropdown on the RSP1A.

## Setup (one-time)

The SDRplay API isn't redistributable, so this crate doesn't bundle
it. Grab the installer from <https://www.sdrplay.com/api/> (free,
account-walled), and run it once:

```bash
chmod +x SDRplay_RSP_API-Linux-ARM64-3.X.run    # ARM (Pi)
# or:    SDRplay_RSP_API-Linux-3.X.run          # x86_64
sudo ./SDRplay_RSP_API-Linux-ARM64-3.X.run      # accept EULA
sudo systemctl enable --now sdrplay
systemctl status sdrplay                         # "active (running)"
```

That installs:
- `/usr/local/lib/libsdrplay_api.so.3.X` (+ symlinks)
- `/usr/local/include/sdrplay_api*.h`
- `/opt/sdrplay_api/sdrplay_apiService` (the daemon)
- `/etc/udev/rules.d/66-sdrplay.rules` (USB permissions)

`build.rs` runs `bindgen` against the C headers at
`/usr/local/include` by default. Override the lookup with
`SDRPLAY_API_INCLUDE_DIR` / `SDRPLAY_API_LIB_DIR` env vars.

End-to-end install procedure for Linux (including troubleshooting and
udev re-tagging) lives in the top-level
[`README.md`](../../README.md#sdrplay-api-install-linux).

## Sample-rate strategy

Project convention (see `sdr-rate-convention.md`): pick rates so
`fs / 48000` is an integer with small-prime factors (2 and 3 only).
The crate targets:

| Stage | Rate | Ratio to 48 kHz |
|---|---|---|
| `PREFERRED_SAMPLE_RATE_HZ` (master) | 2 304 000 Sa/s | × 48 (= 2⁴·3) |
| API internal `decimation = 4` (host) | 576 000 Sa/s | × 12 (= 2²·3) |
| `PolyphaseDecimator` (audio) | 48 000 Sa/s | × 1 |

Both ratios are even multiples of 48 kHz. Same numbers as
`modem-pluto`'s preferred path, so the existing `modem-sdr-dsp` chain
works unchanged.

## Knobs exposed in `SdrplayConfig`

The driver-level config carries every field the API can program. The
backend's `build_sdrplay_config` validates the GUI-side `SdrConfig`
against the per-device capabilities and silently drops fields that
don't apply (e.g. bias-T set to true on an RSP1 — the daemon never
sees it).

| Field | Maps to | Honoured on |
|---|---|---|
| `tuner: Tuner::{A, B}` | `sdrplay_api_DeviceT.tuner` | RSPduo only — single-tuner parts force `Tuner::A` |
| `antenna: AntennaPort::{Hiz, Fifty}` | `RspDuoTunerParams.tuner1AmPortSel` | RSPduo Tuner-A only; ignored elsewhere (single SMA) |
| `bias_t: bool` | RSPduo: `RspDuoTunerParams.biasTEnable`<br>RSP1A/B: `Rsp1aTunerParams.biasTEnable` | RSPduo (tuner B) and RSP1A/B; **no-op on RSP1** |
| `fm_notch: bool` | RSPduo: `RspDuoTunerParams.rfNotchEnable`<br>RSP1A/B: `devParams.rsp1aParams.rfNotchEnable` | RSPduo + RSP1A/B; no-op on RSP1 |
| `dab_notch: bool` | RSPduo: `RspDuoTunerParams.rfDabNotchEnable`<br>RSP1A/B: `devParams.rsp1aParams.rfDabNotchEnable` | RSPduo + RSP1A/B; no-op on RSP1 |
| `rf_freq_hz: u64` | `tunerParams.rfFreq.rfHz` | All — LO frequency, default 145.5 MHz |
| `lna_state: u8` | `tunerParams.gain.LNAstate` | All — VHF table is 10 states on RSPduo/1A/1B (0–9), 4 states on RSP1 (0–3) |
| `if_gain_reduction_db: i32` | `tunerParams.gain.gRdB` | All — IF reduction 20–59 dB. Default 40. Daemon-managed when `agc_mode != Disable` |
| `agc_mode: AgcMode` | `ctrlParams.agc.enable` | All — `Disable` (manual), `Slow` (5 Hz), `Mid` (50 Hz, SDRplay default), `Fast` (100 Hz). LNA stays manual whatever the AGC mode |

## Status

- Phase 1 — driver layer: **done**. `rx::start` produces 48 kHz audio
  via the radio-faithful chain from `modem-sdr-dsp` (QuadratureDemod
  → PolyphaseDecimator → DeemphasisLpf → SubAudioHpf). Validated on
  a real RSPduo at 145 MHz: 240 000 audio samples in 5 s = exact
  48 kHz, 0 frame errors.
- Phase 2 — GUI integration: **done**. `sdrplay:<serial>` device
  entries appear in the RX dropdown next to Pluto and cpal entries;
  `start_capture` routes on the prefix. Settings panel exposes
  every knob in [`SdrplayConfig`] including the AGC mode dropdown
  and the LNA-state input that stays editable under SDRplay AGC
  (only the IF gRdB is daemon-managed).
- Phase 3 — multi-device support: **done** for RSP1 / RSP1A / RSP1B /
  RSPduo. `SdrBackend::capabilities_for(&descriptor)` returns
  per-device caps so the panel hides knobs the part doesn't have.
- Phase 4 — RSP2 / RSPdx / RSPdxR2 wiring: pending. Each needs its
  own `program_params` branch (different antenna structs, HDR mode
  for the dx parts).
- Phase 5 — live retune via `sdrplay_api_Update`: pending. Borrowing
  the band-aware LNA-state → dB table from
  [gr-sdrplay3](https://github.com/fventuri/gr-sdrplay3) for the
  GUI gain slider.

## Smoke test

```bash
cd rust
cargo run -p modem-sdrplay --example probe_demo
```

Opens the first SDRplay on the bus, configures the front-end at
145.5 MHz, streams for ~5 s, prints I/Q sample counts and audio peak.
Useful sanity check after a fresh API install. Works on every
supported device — the example logs which hardware it found.

## Why a custom FFI rather than `soapysdr` + `SoapySDRPlay3`?

Three reasons:

1. **One less moving part**: SoapySDR + SoapySDRPlay3 still link the
   same closed `libsdrplay_api.so` we're calling directly — adding
   Soapy in between buys flexibility we don't need (the modem only
   ever talks to one specific radio at a time).
2. **Consistency with `modem-pluto`**: that crate already does
   direct FFI via `industrial-io`. Same dependency story.
3. **Smaller dep tree on the Pi**: SoapySDRPlay3 isn't packaged in
   Debian 13's repos and would need a manual build alongside the
   API. Direct FFI keeps the Pi setup to "install the SDRplay API
   .run, build the workspace".
