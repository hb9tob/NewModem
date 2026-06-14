# V4 wire format — headerless, marker-bootstrap

Branch: `feat/v3-lms-after-header`
Status: **plan / not yet implemented**
Both ends must update (hard incompatible break with V3 / ≤ 0.13.18).

## 1. Goal

Fold the protocol header into the marker and drop it. Move the LMS warmup to
**after** the bootstrap marker so the RX knows the modulation (`profile_index`)
*before* it trains the FFE on the data constellation.

This kills, structurally, the root cause of the HIGH+ auto-detect bug
(`project_high_highplus_ota_regression`): there is no longer a profile-dependent
block in front of the thing that announces the profile, so the anchor-profile
guessing that corrupted the QPSK header simply no longer exists.

### Why this is the right design (recap of the analysis)

- The **modulation lives in the header today** (`profile_index`, byte 10) — the
  one essential live field. Everything else in the header is redundant or dead
  in V3:
  - `mode_code` → also in AppHeader
  - `payload_length` → redundant with `AppHeader.file_size`; only used in the
    no-AppHeader fallback (`rx_v2.rs:1742`), and u16-capped (vestige)
  - `flags`/`FLAG_EOT` → already secondary to the marker `LAST_FLAG_BIT`
    (0.13.18)
  - `frame_counter`, `freq_offset` → never read in the V3 RX path
- The **marker is the better carrier**: self-equalising via its own 32-sym sync
  LS-gain (robust to residual FFE / phase), already Golay(24,12)+CRC8, and it
  repeats **before every segment** (≈ every 2 CW) instead of once per ~4 s
  preamble block. Putting `profile_index` in it costs **zero extra symbols**
  (the 12-byte payload already maps to 96 syms; we use reserved bytes) and makes
  every segment self-describing → robust re-entry after a channel outage.
- The header was exactly the fragile element that the relay's
  pre-emphasis/limiter + wrong-anchor FFE corrupted. Removing it removes a whole
  fragile decode path.

## 2. Target layout

```
Per preamble block (initial + every V3_PREAMBLE_PERIOD_S ≈ 4 s reinsertion):

  PREAMBLE(256) → MARKER₀(128) → WARMUP(32) → [META_CW + pilots]
                                            → [MARKER(128) → 2 CW + pilots]*

  MARKER₀  = bootstrap/meta marker: carries profile_index + META_FLAG.
             Decoded cold via its own sync LS-gain (profile-agnostic).
  WARMUP   = LMS guard, now built from the profile just read from MARKER₀
             → trained on the CORRECT data constellation.

EOT frame:
  PREAMBLE(256) → MARKER₀(EOT_FRAME_FLAG) → WARMUP(32) → [META_CW] → runout
```

Offsets (fixed, profile-agnostic — good for the gate/auto-detect anchor):
- `MARKER₀`  : [256 .. 384]
- `WARMUP`   : [384 .. 384 + lms_warmup_syms()]  (= [384 .. 416])
- first CW   : [416 ..]

The bootstrap marker is **detached from its meta CW by the warmup** — this is the
one RX special case (see §4): the first marker after a (re)inserted preamble is
followed by `WARMUP` before its CW; all subsequent markers sit directly before
their CW as today.

## 3. New marker payload (still 12 bytes → 96 ctrl syms, unchanged size)

| Field            | Size | Notes |
|------------------|------|-------|
| `seg_id`         | 2 B  | unchanged |
| `session_id_low` | 1 B  | unchanged |
| `base_esi`       | 3 B  | unchanged |
| `flags`          | 1 B  | bit0 META, bit1 LAST, **bit2 EOT_FRAME (new)**, bits3-7 reserved |
| `profile_index`  | 1 B  | **new** (was reserved byte 0) — modulation announce |
| `fmt_version`    | 1 B  | **new** (was reserved byte 1) — wire-format version = 4, for future negotiation |
| `reserved`       | 2 B  | was 4 B → now 2 B, still must be zero |
| `CRC8`           | 1 B  | CCITT over bytes 0..10, unchanged |

Old V3 decoders test only `META_FLAG_BIT` / `LAST_FLAG_BIT` and ignore the rest,
but they will fail anyway (no header where they expect one) — clean hard break.

## 4. Code changes

### TX — `modem-core/src/frame.rs`
- `build_superframe_v3_range`: drop the `header_syms` append; emit
  `PREAMBLE → MARKER₀(profile_index, META) → WARMUP → META_CW`, then the data
  segment loop unchanged. Same for the periodic reinsertion block (lines
  ~271-288).
- `build_eot_frame`: drop header; `MARKER₀` carries `EOT_FRAME` flag; keep
  warmup + meta CW + runout (or trim to PREAMBLE → MARKER₀(EOT) → runout — TBD,
  keep uniform for now).
- `superframe_total_symbols` / `eot_frame_symbols`: replace the `+96` header
  term with the new layout (marker before warmup; header term removed).
- Remove `HEADER_VERSION_V3` usage; introduce `WIRE_FMT_V4 = 4` constant used in
  the marker `fmt_version`.

### RX — `modem-core/src/rx_v2.rs`
- `rx_v3_after`:
  - Remove Phase-1 header decode entirely. Instead: apply a short preamble-only
    FFE pass over `[0 .. 384]`, decode `MARKER₀` via `decode_marker_at` →
    `profile_index` → rebuild `ModemConfig`. **No anchor-profile guessing.**
  - Then the existing Phase-2 FFE pass (preamble + correct warmup refs) over the
    full length. *(Two `apply_ffe` calls remain, but the fragile anchor coupling
    is gone — see §6 for an optional true-single-pass follow-up.)*
  - New offsets: `marker0_start = N_PREAMBLE`, `warmup_start = N_PREAMBLE +
    MARKER_LEN`, `first_cw_start = warmup_start + warmup_len`.
  - Segment walker: when consuming the bootstrap/meta marker (first after a
    preamble), **skip `warmup_len` symbols** before reading its CW. All other
    markers unchanged.
  - `eot_seen`: drop the `decoded_header.flags & FLAG_EOT` term; use
    `saw_last_marker` plus a new `saw_eot_frame` (marker `EOT_FRAME` flag).
  - Payload truncation fallback (`:1742`): no header → use `AppHeader.file_size`
    when present, else ESI-sorted concat without truncation (already the
    no-AppHeader degraded path).
- `estimate_drift_gardner`: update `header_end`-based offsets (lines ~1015,
  1029, 1094) to the new marker/warmup geometry.

### RX worker — `modem-worker/src/rx_worker.rs`
- Replace the `hdr.profile_index` reads (1588/1635/1655) with the bootstrap
  marker's `profile_index`. Drop the auto-detect anchor-pass machinery that
  guessed NORMAL/8PSK.
- `mode_code`/`payload_length` session fields (1923/1924): source from
  `AppHeader` instead of the header.

### `modem-core/src/header.rs`
- Keep the file for now (AppHeader is separate and stays). The protocol `Header`
  struct + `encode/decode_header_symbols` + `derive_profile_index` become dead;
  delete once nothing references them (gate it behind the green test run).

### Gate / auto-detect — `gate.rs`
- No change to preamble detection. The fixed-offset "what modulation" anchor
  moves from header@(256+warmup) to MARKER₀@256. Confirm no other offset
  assumption.

## 5. Tests

Update the offset-hardcoding tests and add new ones:
- `frame.rs`: `superframe_v3_starts_with_preamble_and_header`,
  `eot_frame_carries_eot_flag_in_header`, `superframe_v3_has_marker_after_header`,
  the two `*_matches_build` count tests → rewrite for the V4 layout.
- New: `marker_carries_profile_index_roundtrip`, `v4_bootstrap_marker_at_256`,
  `eot_frame_marked_by_marker_flag`.
- `rx_v2.rs` loopback suite (`loopback_v3_*`, `marker_fit_drift_*`) must stay
  green for **all profiles** — these are the regression guard for HIGH/HIGH++.
- New: `loopback_v4_high_plus_auto_no_anchor` — decode HIGH+ in auto with no
  forced profile, asserting `profile_index` came from the marker.

## 6. Optional follow-up (not in first cut)

True single-pass FFE: modify `ffe::apply_ffe_lms_with_training` to decode
MARKER₀ inline and inject the warmup training refs mid-pass, collapsing the two
`apply_ffe` calls into one. Worth it only if the marker-read mini-pass shows up
in the RX profile; defer until the format change is validated.

## 7. Validation sequence (tests run in parallel by the user)

1. **Offline first** — `modem-cli/examples/probe_wav.rs` extended for V4, run on
   the 3 relay captures in `~/Downloads/nbfm-rx/` (HIGH / HIGH+ / HIGH++).
   Quantify: does HIGH+ now activate in auto with no anchor? Any HIGH++
   regression? (also measures the 64-APSK warmup-adjacency effect.)
2. **Loopback** — full `cargo test -p modem-core` green (NOT `--workspace` on
   this box — thermal). All profiles bit-identical for matched configs.
3. **OTA both-ends** — only after 0.13.18 is confirmed live; coordinate a
   correspondent through the relay with both ends on the V4 build.

## 8. Rollout / risk

- **Hard break**: V4 RX cannot decode V3 TX and vice versa. Old RX fails cleanly
  (no header → no decode), no misdecode. Bump GUI to 0.14.0.
- **Do not stack on unvalidated 0.13.18**: confirm 0.13.18 OTA first.
- **Main regression risk**: HIGH/HIGH++ which work today — guarded by the
  loopback suite + offline replay before any OTA.
- **Open item**: confirm the no-AppHeader fallback degradation (lost
  `payload_length` truncation) is acceptable — it only bites when the AppHeader
  itself failed to decode.

See [[project_high_highplus_ota_regression]], [[feedback_per_profile_rx_buffer]].
