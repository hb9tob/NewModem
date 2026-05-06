# modem-pluto

PlutoSDR (ADALM-PLUTO / AD9363) backend for the NBFM modem. Streams I/Q
through libiio and runs the radio-faithful TX/RX chain from
[`modem-sdr-dsp`](../modem-sdr-dsp). Plugs into `modem-worker` through
the same seams the cpal soundcard backend uses (`SampleSink` for TX,
`mpsc::Receiver<Vec<f32>>` for RX).

> **Status**: scaffold (task #9 of the SDR plan). The crate compiles,
> the module shapes are pinned, every entry point that would touch
> hardware returns `PlutoError::NotImplemented` until tasks #10 and
> #11 land.

## Why Pluto first

- TX is the killer feature — RF loopback (TX SMA → 30 dB pad → RX SMA)
  makes the entire DSP chain ground-truthable in software, no radio
  appliance needed.
- libiio is open and stable (MIT, Analog Devices). No daemon, no
  proprietary install.
- Once the 528-kSa/s ↔ 48-kHz chain is proven here, the same DSP plugs
  straight into RTL-SDR and SDRplay (RX-only, deferred).

## Sample-rate strategy

The modem core is hard-locked to 48 kHz mono f32 audio. To keep the
path single-stage polyphase, the AD9363 is programmed at the **lowest
rate that gives an integer ÷N to 48 kHz**:

| Rate | Ratio | Notes |
|---|---|---|
| **528 kSa/s** | 11 | Preferred. Requires a 4× decimating FIR loaded into `iio:device0/filter_fir_config`. |
| 960 kSa/s | 20 | Fallback if the BBPLL refuses 528. Native AD9363 rate, no FIR-loading dance. |

Both rates are well above Carson's-rule bandwidth for ±5 kHz NBFM
(~16 kHz).

### The mandatory FIR-loading step (528 kSa/s only)

Stock Pluto firmware limits the BBPLL to ~2.083 MS/s without a custom
FIR loaded. The libiio sequence the driver runs at startup is:

```text
write iio:device0/filter_fir_config  ← 4× decimating FIR taps
write in_voltage_filter_fir_en        ← 1
write out_voltage_filter_fir_en       ← 1
write sampling_frequency              ← 528000
```

If the final write returns `EINVAL`, the driver retries at 960 kSa/s.

## Installation

### Linux (Pi 5, Debian Trixie / Ubuntu 24.04)

```bash
sudo apt install libiio-dev
```

(libiio 0.26 confirmed working with `industrial-io = "0.6"` and
`libiio-sys = "0.4"` — no bindgen FFI fallback needed.)

### Windows

The NSIS installer bundles `libiio.dll` (~3 MB) next to the Tauri
sidecar. The Pluto USB driver auto-installs on first plug (signed
CDC-ECM/NCM driver from Analog Devices).

### Network mode

Pluto exposes `ip:pluto.local` (or `ip:192.168.2.1`) over USB-Ethernet
in addition to native USB. Both work; default is `usb:1.6.5` on this
Pi. Override with `--pluto-uri=...` on the CLI.

## Reference values (calibrated against this Pi's Pluto)

- USB libiio URI: `usb:1.6.5`
- Network libiio URI: `ip:pluto.local`
- Firmware: `tezuka-0.1.9` (AD9363 Rev.C)
- Native BBPLL min sample rate (no FIR loaded): 2.083 MS/s
- RX gain range, manual mode: `[-3, 71]` dB step 1
- TX gain (= attenuation) range: `[-89.75, 0]` dB step 0.25
- Buffer-capable RX device: `cf-ad9361-lpc`, format `le:S12/16>>0`
- Buffer-capable TX device: `cf-ad9361-dds-core-lpc`, format
  `le:S16/16>>0`

## DSP chain

The [`modem-sdr-dsp`](../modem-sdr-dsp) crate ships the per-block
math; this crate just orchestrates them. The blocks used here are:

- TX: `interpolator::PolyphaseInterpolator`, then
  `pm_mod::PhaseMod` (radio-faithful PM — built-in +6 dB/oct
  preemphasis matches an NBFM transceiver).
- RX: `decimator::PolyphaseDecimator`, then `fm_demod::QuadratureDemod`,
  then `audio_filters::DeemphasisLpf` and `audio_filters::SubAudioHpf`.

The 35 unit + integration tests in `modem-sdr-dsp` are the regression
baseline these calibration values get tuned against once Pluto-↔-radio
loopback runs.
