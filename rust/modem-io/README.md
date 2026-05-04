# modem-io

Sample IO layer — audio device enumeration, capture, and playback.
Isolates `cpal` (and, later, `libiio` for Pluto-SDR) from `modem-core`
so that adding a new transport does not touch the DSP code.

## Public API

- `SampleSink` trait — `play_buffer(device, sample_rate, samples) ->
  Result<PlaybackHandle, IoError>`. Full-buffer submit (Option A); a
  streaming variant will be added when Pluto / libiio lands.
- `CpalSink` — concrete `SampleSink` backed by `cpal`. Builds the output
  stream lazily inside `play_buffer`.
- `cpal_capture` — input-stream helper for the RX side.
- `devices` — `list_input_devices()` / `list_output_devices()` returning
  `(name, default_sample_rate)` for the GUI dropdowns.

## Trade-off — PTT engagement

`play_buffer` does device lookup + stream build + play in one call. The
worker therefore engages PTT *before* device validation; a missing
device flashes PTT briefly then emits a `tx_error`. This was accepted
to keep the trait simple — the alternative (split open/play methods)
was deferred until a real second backend exists.
