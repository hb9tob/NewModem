# modem-worker

GUI-agnostic TX/RX orchestration. Holds the worker threads, the
on-disk session store, the PTT controller, and the abstract event sink
that the GUI implements.

## Public API

- `EventSink` trait + `EventSinkExt` — the GUI implements this with a
  Tauri `AppHandle`; tests can stub it. Workers never reference Tauri
  directly.
- `tx_worker::run_tx(...)` — synthesises samples in-process via
  `V3Modem::encode_to_samples` and plays them through a `SampleSink`
  (typically `CpalSink`). No subprocess CLI anymore.
- `rx_worker::run_rx(...)` — sliding-window RX over a continuously
  captured sample buffer; emits `rx_*` events for the GUI.
- `session_store::SessionStore` — disk-persistent RX state (24 h TTL),
  one folder per session_id with `meta.json`, `packets.blob`, decoded
  output when complete.
- `ptt::SharedPtt` + `list_ports()` — RTS/DTR-driven PTT over a serial
  port, with configurable polarity per line and a 200 ms guard around
  TX.

## Dependencies

Depends on `modem-core` (for `V3Modem`, `ProfileIndex`, `rx_v2`),
`modem-framing` (for `PayloadEnvelope` / `AppHeader` / RaptorQ), and
`modem-io` (for `CpalSink` and capture). Does not depend on Tauri,
`reqwest`, image crates, or any GUI-only deps.

## What stays out

The Tauri commands, the AVIF encoder, the channel-survey UI, and the
collector HTTP client all live in `modem-gui` — anything tied to the
graphical app or to a specific user-facing flow does not belong here.
