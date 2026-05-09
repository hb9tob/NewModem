# modem-pluto

PlutoSDR (ADALM-PLUTO / AD9363) backend for the NBFM modem. Streams I/Q
through libiio and runs the radio-faithful TX/RX chain from
[`modem-sdr-dsp`](../modem-sdr-dsp). Plugs into `modem-worker` through
the same seams the cpal soundcard backend uses (`SampleSink`-shaped
TX, `mpsc::Receiver<Vec<f32>>` RX).

## Why Pluto first

- TX is the killer feature — RF loopback (TX SMA → 30 dB pad → RX SMA,
  or AD9361 FPGA-internal digital loopback) makes the entire DSP chain
  ground-truthable in software, no radio appliance needed.
- libiio is open and stable (MIT, Analog Devices). No daemon, no
  proprietary install.
- Once the 576-kSa/s ↔ 48-kHz chain is proven here, the same DSP plugs
  straight into RTL-SDR and SDRplay (RX-only, deferred).

## Sample-rate strategy

The modem core is hard-locked to 48 kHz mono f32 audio. Project
convention is to pick SDR rates so the integer ÷N to 48 kHz is a
**small-prime composite** — only 2s and 3s, no odd factors and no
primes above 3. That lets the polyphase resamplers decompose into
cheap multi-stage half-band filters and matches the AD9361's own
internal HB chain.

| Rate | Ratio | Factors | Notes |
|---|---|---|---|
| **576 kSa/s** | 12 | 2² · 3 | Preferred. Requires a 4× FIR loaded into `iio:device0/filter_fir_config` (the AD9361 BBPLL floors at 2.083 MS/s without it). |
| 2304 kSa/s | 48 | 2⁴ · 3 | Fallback if the BBPLL refuses 576. Sits above the 2.083 MS/s native floor so no FIR-loading dance is strictly required. |

Both rates are well above Carson's-rule bandwidth for ±5 kHz NBFM
(~16 kHz).

### The mandatory FIR-loading step (576 kSa/s only)

Stock Pluto firmware limits the BBPLL to ~2.083 MS/s without a custom
FIR loaded. The libiio sequence the driver runs at startup is:

```text
write iio:device0/filter_fir_config  ← 4× decimating FIR taps
write in_voltage_filter_fir_en        ← 1
write out_voltage_filter_fir_en       ← 1
write sampling_frequency              ← 576000
```

If the final write returns `EINVAL`, the driver retries at 2304 kSa/s.

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
- RX: `fm_demod::QuadratureDemod`, then
  `decimator::PolyphaseDecimator`, then
  `audio_filters::DeemphasisLpf` and `audio_filters::SubAudioHpf`.

## Running the live tests

```bash
# Verify device::open and FIR loading on the plugged-in Pluto:
cargo run -p modem-pluto --example probe_open

# Full end-to-end loopback (uses AD9361 FPGA digital loopback,
# no RF cable needed): pushes a 1 kHz tone through TX,
# captures it through RX, asserts ZC-rate matches:
cargo run -p modem-pluto --example loopback_demo
# Captured audio is dumped to /tmp/pluto_loopback.wav for inspection.
```
