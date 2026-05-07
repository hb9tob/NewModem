# modem-sdrplay — SDRplay RSPduo backend (RX-only)

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

TX is out of scope — RSPduo is RX-only hardware.

## Setup (one-time)

The SDRplay API isn't redistributable, so this crate doesn't bundle
it. Grab the installer from <https://www.sdrplay.com/api/> (free,
account-walled), and run it once:

```bash
chmod +x SDRplay_RSP_API-Linux-ARM64-3.X.run
sudo ./SDRplay_RSP_API-Linux-ARM64-3.X.run    # accept EULA
sudo systemctl enable --now sdrplay
systemctl status sdrplay                       # "active (running)"
```

That installs:
- `/usr/local/lib/libsdrplay_api.so.3.X` (+ symlinks)
- `/usr/local/include/sdrplay_api*.h`
- `/opt/sdrplay_api/sdrplay_apiService` (the daemon)
- `/etc/udev/rules.d/66-sdrplay.rules` (USB permissions)

`build.rs` runs `bindgen` against the C headers at
`/usr/local/include` by default. Override the lookup with
`SDRPLAY_API_INCLUDE_DIR` / `SDRPLAY_API_LIB_DIR` env vars.

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

## RSPduo-specific knobs (exposed in `SdrplayConfig`)

| Field | Maps to | Notes |
|---|---|---|
| `tuner: Tuner::{A, B}` | `sdrplay_api_DeviceT.tuner` | Single-tuner mode |
| `antenna: AntennaPort::{Hiz, Fifty}` | `RspDuoTunerParams.tuner1AmPortSel` | Tuner-A only; Tuner-B is hard-wired 50 Ω |
| `bias_t: bool` | `RspDuoTunerParams.biasTEnable` | **Tuner B only** — +5 V on the antenna port for external preamps |
| `fm_notch: bool` | `RspDuoTunerParams.rfNotchEnable` | Broadcast-FM band-stop (~88–108 MHz) |
| `dab_notch: bool` | `RspDuoTunerParams.rfDabNotchEnable` | DAB band-III stop (~174–240 MHz) |
| `rf_freq_hz: u64` | `tunerParams.rfFreq.rfHz` | LO frequency, default 145.5 MHz |
| `lna_state: u8` | `tunerParams.gain.LNAstate` | 0 = least atten / most gain. RSPduo VHF table has 10 states |
| `if_gain_reduction_db: i32` | `tunerParams.gain.gRdB` | IF reduction, range 20–59. Default 40. |

## Status

- Phase 1 — driver layer: **done**. `rx::start` produces 48 kHz audio
  via the radio-faithful chain from `modem-sdr-dsp` (QuadratureDemod
  → PolyphaseDecimator → DeemphasisLpf → SubAudioHpf). Validated on
  a real RSPduo at 145 MHz: 240 000 audio samples in 5 s = exact
  48 kHz, 0 frame errors.
- Phase 2 — GUI integration: pending (mirror Pluto's `pluto:` device
  prefix, settings fieldset, freq keypad share).
- Phase 3 — live retune via `sdrplay_api_Update`: pending. Borrowing
  the band-aware LNA-state → dB table from
  [gr-sdrplay3](https://github.com/fventuri/gr-sdrplay3) for the
  GUI gain slider.

## Smoke test

```bash
cd rust
cargo run -p modem-sdrplay --example probe_demo
```

Opens the first RSPduo on the bus, configures Tuner B at 145.5 MHz,
streams for ~5 s, prints I/Q sample counts and audio peak. Useful
sanity check after a fresh API install.

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
