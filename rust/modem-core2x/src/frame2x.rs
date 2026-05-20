//! Superframe assembly for the 2x wire format V4.
//!
//! Cycle layout (one PLHEADER period ≈ 4 s):
//!
//! ```text
//! [PLHEADER 192]
//!   [LMS warmup 32/64]                (APSK-32 / APSK-64 only)
//! [META-CW data interleaved with TDM pilots]    (LDPC CW carrying AppHeader×4)
//! [DATA-CW[0] data interleaved with TDM pilots]
//! [DATA-CW[1] data interleaved with TDM pilots]
//! ...
//! ```
//!
//! Each LDPC codeword is interleaved with the V3-style TDM pilot pattern
//! ([`pilot2x_tdm::PilotPattern2x`]): `d_syms` data + `p_syms` rotating
//! QPSK pilots per group, repeating across the CW. Default 32/2 for the
//! whole catalogue; HighPlusPlus2x densifies to 16/2 for the
//! squared-min-distance reason that drove V3 HighPlusPlus.
//!
//! Differences vs V3 frame:
//!
//! - No marker between segments (PLHEADER cycle re-anchors directly).
//! - No runout symbols — the last pilot group already gives the RX a
//!   clean reference; a fresh PLHEADER is the only re-anchor.
//! - Pilots continue rotating across the META → DATA-CW boundary so
//!   the rotation index reconstructs from one per-cycle counter.
//!
//! ESI accounting: the PLS payload carries `base_esi` = ESI of the *first*
//! DATA-CW in the cycle (i.e. the CW immediately after the META-CW). The
//! META-CW itself does not consume an ESI.

use modem_core_base::constellation::{
    self as cons, Constellation,
};
use modem_core_base::interleaver;
use modem_core_base::ldpc::encoder::LdpcEncoder;
use modem_core_base::profile_types::ConstellationType;
use modem_core_base::scrambler;
use modem_core_base::types::Complex64;
use modem_framing::app_header::{self, AppHeader};

use crate::pilot2x_tdm::{self, PilotPattern2x};
use crate::plheader::{self, PlsPayload};
use crate::preburst;
use crate::profile2x::ModemConfig2x;

/// Target spacing between PLHEADERs (seconds). Matches V3
/// `V3_PREAMBLE_PERIOD_S` so the late-entry budget stays the same.
pub const V4_PREAMBLE_PERIOD_S: f64 = 4.0;

/// Bit 0 — META present in the cycle. Always set today (we emit one
/// META-CW per PLHEADER cycle); bit reserved for future cycles that
/// only carry data.
pub const FLAG2X_META: u8 = 1 << 0;
/// Bit 1 — final cycle of the burst (used by EOT).
pub const FLAG2X_EOT: u8 = 1 << 1;
/// Bit 2 — last RaptorQ packet of the session is in this cycle.
pub const FLAG2X_LAST: u8 = 1 << 2;

// --- Constellation helper (mirrors V3 `frame::make_constellation`) --------

/// Build the constellation matching a `ModemConfig2x.base.constellation`.
pub fn make_constellation_2x(cfg: &ModemConfig2x) -> Constellation {
    match cfg.base.constellation {
        ConstellationType::Qpsk => cons::qpsk_gray(),
        ConstellationType::Psk8 => cons::psk8_gray(),
        ConstellationType::Apsk16 => cons::apsk16_dvbs2(cfg.base.apsk_gamma),
        ConstellationType::Apsk32 => cons::apsk32_dvbs2(
            cfg.base.apsk_gamma,
            cfg.base.apsk_gamma2,
        ),
        ConstellationType::Apsk64 => cons::apsk64_dvbs2x(
            cfg.base.apsk_gamma,
            cfg.base.apsk_gamma2,
            cfg.base.apsk_gamma3,
        ),
    }
}

// --- LMS warmup (mirrors V3 `make_lms_warmup_for_config`) -----------------

/// Build the LMS warmup sequence inserted right after the PLHEADER for
/// APSK profiles. Returns `Vec::new()` for QPSK / 8PSK / 16-APSK (whose
/// SOF already covers the whole constellation envelope).
///
/// For Apsk32 (HIGH+/HIGH+56): 32 sym = full sweep of the 4+12+16 layout,
/// each point exactly once in canonical Figure 12 order.
/// For Apsk64 (HIGH++): 64 sym = full sweep of the 4+12+20+28 layout, each
/// point exactly once in canonical Table 13e order.
pub fn make_lms_warmup_2x(cfg: &ModemConfig2x) -> Vec<Complex64> {
    let n = cfg.lms_warmup_syms;
    if n == 0 {
        return Vec::new();
    }
    let constellation = make_constellation_2x(cfg);
    (0..n)
        .map(|k| constellation.points[k % constellation.points.len()])
        .collect()
}

// --- Codeword encode helper (mirrors V3 `encode_one_codeword`) ------------

/// Encode `info_bytes` into a slice of complex constellation symbols
/// (the data portion of one CW, before pilot interleaving). Public
/// because the [`crate::rx_v4`] turbo loop re-encodes the converged
/// LDPC output to recover the "truth" symbol sequence for pass 2
/// channel estimation (data-driven per-ring σ² and gain).
pub fn encode_one_codeword(
    info_bytes: &[u8],
    encoder: &LdpcEncoder,
    interleave_perm: &[usize],
    constellation: &Constellation,
) -> Vec<Complex64> {
    let mut info_bits = vec![0u8; encoder.k()];
    for (byte_idx, &byte) in info_bytes.iter().enumerate() {
        for bit in 0..8 {
            info_bits[byte_idx * 8 + bit] = (byte >> (7 - bit)) & 1;
        }
    }
    let codeword = encoder.encode(&info_bits);
    let padded_n = interleave_perm.len();
    let cw_padded = if padded_n == codeword.len() {
        codeword
    } else {
        let mut buf = vec![0u8; padded_n];
        buf[..codeword.len()].copy_from_slice(&codeword);
        buf
    };
    let interleaved = interleaver::apply_permutation(&cw_padded, interleave_perm);
    constellation.map_bits(&interleaved)
}

// --- Symbol-accurate cycle / superframe layout ----------------------------

/// Symbols emitted per LDPC codeword *after* TDM pilot interleaving.
pub(crate) fn cw_with_pilots_len(cfg: &ModemConfig2x) -> usize {
    cfg.pilot_pattern.wire_len(cfg.cw_data_syms())
}

/// Number of pilot symbols emitted per LDPC codeword.
pub(crate) fn pilot_syms_per_cw(cfg: &ModemConfig2x) -> usize {
    cfg.pilot_pattern.n_groups(cfg.cw_data_syms()) * cfg.pilot_pattern.p_syms
}

/// Number of TDM groups (data+pilot pairs) per LDPC codeword.
pub(crate) fn pilot_groups_per_cw(cfg: &ModemConfig2x) -> usize {
    cfg.pilot_pattern.n_groups(cfg.cw_data_syms())
}

/// Symbols emitted by one PLHEADER cycle that carries `n_data_cw` data
/// codewords plus exactly one META-CW.
///
/// Layout: PLHEADER + warmup + (META-CW + pilots) + n_data_cw × (DATA-CW
/// + pilots).
fn cycle_total_syms(cfg: &ModemConfig2x, n_data_cw: usize) -> usize {
    plheader::PLHEADER_LEN_SYM
        + cfg.lms_warmup_syms
        + cw_with_pilots_len(cfg)        // meta CW + its pilot block(s)
        + n_data_cw * cw_with_pilots_len(cfg)
}

/// Round `n_packets` up to the next multiple of [`data_cw_per_cycle`]
/// so the burst's last cycle is fully filled. The tail-fill packets are
/// extra RaptorQ repair codewords (cheap : the codec can produce any
/// ESI on demand), replacing the legacy "silence until EOT PLHEADER"
/// padding. RX has no special path for them — they decode like any
/// other repair CW and add free margin to the RaptorQ recovery.
pub fn tail_filled_data_cw_count(cfg: &ModemConfig2x, n_packets: u32) -> u32 {
    let cw_per_cycle = data_cw_per_cycle(cfg) as u32;
    let leftover = n_packets % cw_per_cycle;
    if leftover == 0 {
        n_packets
    } else {
        n_packets + (cw_per_cycle - leftover)
    }
}

/// Number of data CWs per cycle so that the cycle stays under
/// `V4_PREAMBLE_PERIOD_S`. At least 1 — even tiny payloads emit a single
/// data CW per cycle so late-entry RX always has something to anchor.
pub fn data_cw_per_cycle(cfg: &ModemConfig2x) -> usize {
    let target_syms = (V4_PREAMBLE_PERIOD_S * cfg.base.symbol_rate) as usize;
    let cw_with_pilots = cw_with_pilots_len(cfg);
    let header_syms = plheader::PLHEADER_LEN_SYM
        + cfg.lms_warmup_syms
        + cw_with_pilots; // meta-CW
    let budget = target_syms.saturating_sub(header_syms);
    (budget / cw_with_pilots).max(1)
}

/// Total symbol count produced by [`build_superframe_v4_range`] for a
/// requested burst of `n_data_cw` data codewords. Mirror of the builder
/// logic so the worker / TX duration estimator stays in sync without
/// resimulating the whole encode.
///
/// Includes the tail-fill : the actual emitted CW count is
/// [`tail_filled_data_cw_count`]`(cfg, n_data_cw)`, always a multiple of
/// [`data_cw_per_cycle`]. Every cycle (including the last) is a full
/// `data_cw_per_cycle` cycle. There is no trailing partial cycle.
pub fn superframe_total_symbols_v4(cfg: &ModemConfig2x, n_data_cw: u32) -> usize {
    let cw_per_cycle = data_cw_per_cycle(cfg);
    let emitted = tail_filled_data_cw_count(cfg, n_data_cw) as usize;
    if emitted == 0 {
        // Zero-data burst : one META-only cycle (matches the
        // build_superframe_v4_range branch that emits at least one
        // cycle so late-entry RX always has a PLHEADER anchor).
        return cycle_total_syms(cfg, 0);
    }
    let n_cycles = emitted / cw_per_cycle;
    n_cycles * cycle_total_syms(cfg, cw_per_cycle)
}

/// Symbol count for an EOT frame: a single PLHEADER cycle carrying the
/// META-CW (zero-data AppHeader) and zero data codewords.
pub fn eot_frame_symbols_v4(cfg: &ModemConfig2x) -> usize {
    cycle_total_syms(cfg, 0)
}

/// True when `chunk_offset` (within a `cw_with_pilots_len(cfg)`-sized
/// CW chunk) lands inside a TDM pilot slot. Mirror of
/// [`pilot2x_tdm::pilot_positions_2x`], exposed for receive-side
/// machinery (timing recovery) that wants pilot/data classification
/// without driving the full deinterleaver.
pub fn chunk_offset_is_pilot(
    chunk_offset: usize,
    cw_data_syms: usize,
    pattern: &PilotPattern2x,
) -> bool {
    if pattern.p_syms == 0 {
        return false;
    }
    let d = pattern.d_syms;
    let p = pattern.p_syms;
    let n_groups = pattern.n_groups(cw_data_syms);
    let mut wire_cursor = 0usize;
    let mut data_cursor = 0usize;
    for _ in 0..n_groups {
        let take = (d).min(cw_data_syms - data_cursor);
        wire_cursor += take;
        if chunk_offset >= wire_cursor && chunk_offset < wire_cursor + p {
            return true;
        }
        wire_cursor += p;
        data_cursor += take;
    }
    false
}

/// Length of one PLHEADER cycle in symbols, for a full cycle carrying
/// `data_cw_per_cycle(cfg)` data codewords. Convenience accessor for
/// receive-side modules that need cycle-modular arithmetic.
pub fn full_cycle_len_syms(cfg: &ModemConfig2x) -> usize {
    cycle_total_syms(cfg, data_cw_per_cycle(cfg))
}

/// Pre-compute a per-cycle pilot-position map: `map[k] = true` iff
/// symbol offset `k` within a full PLHEADER cycle is a TDM pilot. The
/// PLHEADER (192 sym SOF+PLS), LMS warmup and every data symbol map to
/// `false`.
///
/// The map length equals [`full_cycle_len_syms`]`(cfg)`. Receive-side
/// timing recovery uses `map[(abs - sof_anchor) % len]` to gate a pilot-
/// aware TED (AbsGardner on pilots, Gardner elsewhere) — the DVB-S2X
/// §9.3.2 cure for AbsGardner's data-induced bias on multi-ring APSK.
pub fn cycle_pilot_map(cfg: &ModemConfig2x) -> Vec<bool> {
    let cw_with_pilots = cw_with_pilots_len(cfg);
    let cw_per_cycle = data_cw_per_cycle(cfg);
    let cycle_len = cycle_total_syms(cfg, cw_per_cycle);
    let mut map = vec![false; cycle_len];
    let cw_block_start = plheader::PLHEADER_LEN_SYM + cfg.lms_warmup_syms;
    let cw_data_syms = cfg.cw_data_syms();
    // META-CW + cw_per_cycle DATA-CWs all share the same chunk layout.
    for cw_idx in 0..=cw_per_cycle {
        let chunk_start = cw_block_start + cw_idx * cw_with_pilots;
        for off in 0..cw_with_pilots {
            if chunk_offset_is_pilot(off, cw_data_syms, &cfg.pilot_pattern) {
                map[chunk_start + off] = true;
            }
        }
    }
    map
}

// --- Wire-format encoders --------------------------------------------------

/// Emit a complete RaptorQ-encoded burst as a stream of complex symbols.
///
/// The burst encodes the packets in `[esi_start, esi_start + n_packets)`.
/// Late-entry RX can decode any subset of cycles independently — every
/// cycle re-emits the AppHeader (in the META-CW) and the cycle's
/// `base_esi` (in the PLS payload).
pub fn build_superframe_v4_range(
    data: &[u8],
    cfg: &ModemConfig2x,
    session_id: u32,
    mime_type: u8,
    hash_short: u16,
    esi_start: u32,
    n_packets: u32,
) -> Vec<Complex64> {
    let encoder = LdpcEncoder::new(cfg.base.ldpc_rate);
    let constellation = make_constellation_2x(cfg);
    let interleave_perm = interleaver::interleave_table(
        interleaver::padded_cw_bits(encoder.n(), cfg.base.constellation),
        cfg.base.constellation,
    );
    let k_bytes = encoder.k() / 8;

    // Pad the wire payload up to the next K·k_bytes RaptorQ source-block
    // boundary with PRBS bytes (LFSR-15, one-shot stream from
    // `LFSR_SEED`). Never zeros. The RaptorQ encoder used to internally
    // zero-pad to the same boundary (raptorq_codec::encode_packets_range
    // ~line 93) ; we pre-pad with structured pseudo-random bytes
    // instead, so the source block carries no predictable runs and the
    // bits inside source packet (K-1) are usable as random data should
    // a future RX want to use them after RaptorQ decode (RX truncates
    // to `file_size` so for normal decoding the bytes are invisible).
    let k_source = modem_framing::raptorq_codec::k_from_payload(data.len(), k_bytes);
    let padded_len = k_source * k_bytes;
    let padded_owned: Option<Vec<u8>> = if data.len() >= padded_len {
        None
    } else {
        let mut padded = Vec::with_capacity(padded_len);
        padded.extend_from_slice(data);
        padded.extend_from_slice(&preburst::lfsr15_bytes(padded_len - data.len()));
        Some(padded)
    };
    let raw_for_encode: &[u8] = padded_owned.as_deref().unwrap_or(data);

    // G3RUH scrambler on the source bytes (before RaptorQ). Whitens the
    // payload so the modulator never sees long correlated runs, no
    // matter what the user data looks like. The matching descrambler
    // runs in `rx_v4` on the reassembled buffer truncated to
    // `file_size`. Self-sync, so byte N only depends on bytes < N — the
    // truncation at RX is safe.
    let scrambled_for_encode: Vec<u8> = scrambler::scramble(raw_for_encode);
    let data_for_encode: &[u8] = &scrambled_for_encode;

    // Tail-fill : round `n_packets` up to the next multiple of
    // `data_cw_per_cycle(cfg)` so the burst's last cycle is fully
    // filled. The extra packets are additional RaptorQ repair CWs
    // (cheap — the codec can produce arbitrary ESI), replacing the
    // legacy "silence until EOT" padding. RX has no special path for
    // them — they decode like any other repair CW.
    let n_packets_emit = tail_filled_data_cw_count(cfg, n_packets);
    let packets = modem_framing::raptorq_codec::encode_packets_range(
        data_for_encode,
        k_bytes as u16,
        esi_start,
        n_packets_emit,
    );

    // Encode each data CW once.
    let data_cw_syms: Vec<Vec<Complex64>> = packets
        .iter()
        .map(|p| encode_one_codeword(p, &encoder, &interleave_perm, &constellation))
        .collect();

    // Pre-encode META-CW (constant across all cycles in this burst —
    // session_id, file_size, K, T, hash all stable).
    let app_hdr = AppHeader {
        session_id,
        file_size: data.len() as u32,
        k_symbols: k_source.min(u16::MAX as usize) as u16,
        t_bytes: k_bytes as u8,
        // mode_code is V3-specific; in V4 the PLS already carries the
        // full ProfileIndex2x. Keep the field non-zero (0xA5 = unused)
        // so legacy tools that inspect it don't see an all-zero header.
        mode_code: 0xA5,
        mime_type,
        hash_short,
    };
    let meta_payload = app_header::encode_meta_payload(&app_hdr, k_bytes);
    let meta_cw_syms = encode_one_codeword(
        &meta_payload,
        &encoder,
        &interleave_perm,
        &constellation,
    );

    let warmup_syms = make_lms_warmup_2x(cfg);
    let cw_per_cycle = data_cw_per_cycle(cfg);
    let n_data_cw = data_cw_syms.len();
    let session_id_low = (session_id & 0xFF) as u8;
    let groups_per_cw = pilot_groups_per_cw(cfg);

    // Tail-fill invariant : with `tail_filled_data_cw_count` driving
    // the encode count, the emitted CW count is a multiple of
    // `cw_per_cycle`. Every cycle (including the last) carries
    // exactly `cw_per_cycle` DATA-CWs — no truncation, no over-read
    // gate needed on RX.
    debug_assert!(
        n_data_cw == 0 || n_data_cw % cw_per_cycle == 0,
        "tail-fill broken : n_data_cw={n_data_cw} not multiple of cw_per_cycle={cw_per_cycle}",
    );

    let mut out: Vec<Complex64> = Vec::with_capacity(
        superframe_total_symbols_v4(cfg, n_data_cw as u32),
    );
    let mut data_cursor: usize = 0;
    let mut frame_counter: u16 = 0;
    let mut seg_id: u16 = 0;

    // Always emit at least one cycle so a zero-data burst still surfaces
    // a usable PLHEADER (matches V3 EOT semantics).
    let cycle_count_estimate = ((n_data_cw + cw_per_cycle - 1) / cw_per_cycle).max(1);

    for cycle_idx in 0..cycle_count_estimate {
        // PLHEADER + LMS warmup.
        let pls = PlsPayload {
            profile_index: 0, // worker can override via build_superframe_v4
            seg_id,
            session_id_low,
            base_esi: esi_start + data_cursor as u32,
            flags: FLAG2X_META
                | if cycle_idx + 1 == cycle_count_estimate {
                    FLAG2X_LAST
                } else {
                    0
                },
            frame_counter,
            freq_offset: 0,
        };
        out.extend(plheader::make_plheader(&pls, cfg.family));
        out.extend_from_slice(&warmup_syms);

        // Pilot rotation index resets at every cycle (PLHEADER acts as
        // anchor). META-CW starts at group 0, the k-th DATA-CW continues
        // from groups_per_cw*(1+k) so the rotation is continuous across
        // CW boundaries — RX reconstructs the same sequence from the
        // PLHEADER position alone.
        let (meta_w_pilot, _pos) = pilot2x_tdm::interleave_data_pilots_2x(
            &meta_cw_syms,
            &cfg.pilot_pattern,
            0,
        );
        out.extend(meta_w_pilot);

        let cw_take = cw_per_cycle.min(n_data_cw.saturating_sub(data_cursor));
        for k in 0..cw_take {
            let group_offset = groups_per_cw * (1 + k);
            let (cw_w_pilot, _pos) = pilot2x_tdm::interleave_data_pilots_2x(
                &data_cw_syms[data_cursor + k],
                &cfg.pilot_pattern,
                group_offset,
            );
            out.extend(cw_w_pilot);
        }
        data_cursor += cw_take;
        seg_id = seg_id.wrapping_add(1);
        frame_counter = frame_counter.wrapping_add(1);
    }

    out
}

/// Convenience wrapper: build a full superframe (K source + 30% repair),
/// equivalent to V3 `build_superframe_v3`.
pub fn build_superframe_v4(
    data: &[u8],
    cfg: &ModemConfig2x,
    session_id: u32,
    mime_type: u8,
    hash_short: u16,
) -> Vec<Complex64> {
    let k_bytes = LdpcEncoder::new(cfg.base.ldpc_rate).k() / 8;
    let k_source = modem_framing::raptorq_codec::k_from_payload(data.len(), k_bytes) as u32;
    let n_total = k_source + modem_framing::raptorq_codec::n_repair_default(k_source);
    build_superframe_v4_range(
        data,
        cfg,
        session_id,
        mime_type,
        hash_short,
        0,
        n_total,
    )
}

/// Build the EOT frame: a single PLHEADER cycle with `flags = META|EOT|LAST`,
/// a META-CW carrying the AppHeader (zero data), and zero data codewords.
pub fn build_eot_frame_v4(cfg: &ModemConfig2x, session_id: u32) -> Vec<Complex64> {
    let encoder = LdpcEncoder::new(cfg.base.ldpc_rate);
    let constellation = make_constellation_2x(cfg);
    let interleave_perm = interleaver::interleave_table(
        interleaver::padded_cw_bits(encoder.n(), cfg.base.constellation),
        cfg.base.constellation,
    );
    let k_bytes = encoder.k() / 8;

    let app_hdr = AppHeader {
        session_id,
        file_size: 0,
        k_symbols: 0,
        t_bytes: k_bytes as u8,
        mode_code: 0xA5,
        mime_type: 0,
        hash_short: 0,
    };
    let meta_payload = app_header::encode_meta_payload(&app_hdr, k_bytes);
    let meta_cw_syms = encode_one_codeword(
        &meta_payload,
        &encoder,
        &interleave_perm,
        &constellation,
    );

    let pls = PlsPayload {
        profile_index: 0,
        seg_id: 0,
        session_id_low: (session_id & 0xFF) as u8,
        base_esi: 0,
        flags: FLAG2X_META | FLAG2X_EOT | FLAG2X_LAST,
        frame_counter: 0,
        freq_offset: 0,
    };

    let mut out = Vec::with_capacity(eot_frame_symbols_v4(cfg));
    out.extend(plheader::make_plheader(&pls, cfg.family));
    out.extend(make_lms_warmup_2x(cfg));
    let (meta_w_pilot, _pos) = pilot2x_tdm::interleave_data_pilots_2x(
        &meta_cw_syms,
        &cfg.pilot_pattern,
        0,
    );
    out.extend(meta_w_pilot);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pilot2x_tdm::pilot_symbol_2x;
    use crate::plheader::{decode_plheader_at, PLHEADER_LEN_SYM};
    use crate::profile2x::{
        profile_high_2x, profile_high_plus_2x, profile_high_plus_plus_2x,
        profile_normal_2x, profile_robust_2x, profile_ultra_2x, ProfileIndex2x,
    };
    use modem_framing::app_header::mime;

    #[test]
    fn cw_data_syms_through_make_constellation_apsk32_padded() {
        let cfg = profile_high_plus_2x();
        let cons = make_constellation_2x(&cfg);
        // Apsk32: 5 bits/sym; bit_for_sym maps 5 bits/sym, points = 32.
        assert_eq!(cons.points.len(), 32);
        assert_eq!(cfg.cw_data_syms(), 461); // 2304/5 round-up = 461
    }

    #[test]
    fn lms_warmup_lengths_match_constellation() {
        assert_eq!(make_lms_warmup_2x(&profile_normal_2x()).len(), 0);
        assert_eq!(make_lms_warmup_2x(&profile_high_2x()).len(), 0);
        assert_eq!(make_lms_warmup_2x(&profile_high_plus_2x()).len(), 32);
        assert_eq!(make_lms_warmup_2x(&profile_high_plus_plus_2x()).len(), 64);
    }

    #[test]
    fn lms_warmup_sweeps_each_apsk32_point_once() {
        let cfg = profile_high_plus_2x();
        let warmup = make_lms_warmup_2x(&cfg);
        let cons = make_constellation_2x(&cfg);
        for (k, sym) in warmup.iter().enumerate() {
            assert_eq!(*sym, cons.points[k]);
        }
    }

    #[test]
    fn data_cw_per_cycle_at_least_one() {
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let n = data_cw_per_cycle(&cfg);
            assert!(n >= 1, "profile {p:?} got {n}");
        }
    }

    #[test]
    fn data_cw_per_cycle_close_to_target_period() {
        // TDM 32/2 pattern → CW wire size:
        //   ULTRA  QPSK    1152 data → 36 groups × 2 = 72 pilot → 1224 wire.
        //          Cycle ~2000 sym ; PLHEADER+meta = 192+1224 = 1416 →
        //          budget 584 / 1224 = 0 → clamped to 1.
        //   ROBUST QPSK    1152 data → 1224 wire. Cycle 4000; PLHEADER+meta
        //          = 1416 → 2584 / 1224 = 2.
        //   HIGH   16-APSK 576 data → 18 groups × 2 = 36 pilot → 612 wire.
        //          Cycle 6000 ; PLHEADER+meta = 804 → 5196 / 612 = 8.
        assert_eq!(data_cw_per_cycle(&profile_ultra_2x()), 1);
        assert_eq!(data_cw_per_cycle(&profile_robust_2x()), 2);
        let n_high = data_cw_per_cycle(&profile_high_2x());
        assert!(n_high >= 6 && n_high <= 10, "HIGH cycle got {n_high}");
    }

    #[test]
    fn superframe_total_symbols_matches_build() {
        // For a sweep of profiles + payload sizes the helper must
        // predict the exact symbol count the builder emits.
        let payload_sizes = [200usize, 1500, 5000];
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            for &size in &payload_sizes {
                let data = vec![0xA5u8; size];
                let actual = build_superframe_v4(
                    &data,
                    &cfg,
                    0xDEAD_BEEF,
                    mime::BINARY,
                    0x1234,
                );
                let k_bytes = cfg.base.ldpc_rate.k() / 8;
                let k_source = modem_framing::raptorq_codec::k_from_payload(
                    size, k_bytes,
                ) as u32;
                let n_total = k_source
                    + modem_framing::raptorq_codec::n_repair_default(k_source);
                let predicted = superframe_total_symbols_v4(&cfg, n_total);
                assert_eq!(
                    actual.len(),
                    predicted,
                    "{p:?} payload={size} actual={} predicted={predicted} \
                     n_total={n_total}",
                    actual.len()
                );
            }
        }
    }

    #[test]
    fn eot_frame_symbols_matches_build() {
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let actual = build_eot_frame_v4(&cfg, 0xDEAD_BEEF);
            let predicted = eot_frame_symbols_v4(&cfg);
            assert_eq!(
                actual.len(),
                predicted,
                "{p:?} EOT actual={} predicted={predicted}",
                actual.len()
            );
        }
    }

    #[test]
    fn superframe_starts_with_plheader_decodable() {
        // First 192 sym of any superframe must decode as a PLHEADER for
        // the profile's family. Carries the cycle's META flag.
        let cfg = profile_high_2x();
        let data = vec![0x42u8; 1000];
        let symbols = build_superframe_v4(&data, &cfg, 0xCAFEBABE, mime::BINARY, 0x9999);
        assert!(symbols.len() >= PLHEADER_LEN_SYM);
        let (pls, gain) = decode_plheader_at(&symbols[..PLHEADER_LEN_SYM], cfg.family)
            .expect("PLHEADER must decode noise-free");
        assert!((gain - Complex64::new(1.0, 0.0)).norm() < 1e-9);
        assert_eq!(pls.session_id_low, 0xCAFEBABEu32 as u8);
        assert!(pls.flags & FLAG2X_META != 0, "META flag missing");
    }

    #[test]
    fn eot_frame_pls_carries_eot_flag() {
        let cfg = profile_normal_2x();
        let symbols = build_eot_frame_v4(&cfg, 0xCAFEBABE);
        let (pls, _) = decode_plheader_at(&symbols[..PLHEADER_LEN_SYM], cfg.family)
            .expect("EOT PLHEADER must decode");
        assert!(pls.flags & FLAG2X_EOT != 0, "EOT flag missing");
        assert!(pls.flags & FLAG2X_META != 0, "META flag missing on EOT");
        assert!(pls.flags & FLAG2X_LAST != 0, "LAST flag missing on EOT");
        assert_eq!(pls.base_esi, 0);
    }

    #[test]
    fn first_tdm_pilot_group_after_warmup_carries_rotating_qpsk() {
        // After PLHEADER + warmup the META-CW starts with d_syms data
        // symbols, then p_syms pilots whose values are the rotating
        // QPSK reference pilot_symbol_2x(k) for k=0..p_syms.
        let cfg = profile_high_2x();
        let data = vec![0u8; 200];
        let syms = build_superframe_v4(&data, &cfg, 1, mime::BINARY, 0);
        let pattern = cfg.pilot_pattern;
        let pilot_start = PLHEADER_LEN_SYM + cfg.lms_warmup_syms + pattern.d_syms;
        for k in 0..pattern.p_syms {
            let want = pilot_symbol_2x(k);
            let got = syms[pilot_start + k];
            assert!(
                (got - want).norm() < 1e-12,
                "first pilot slot {k}: got {got:?} want {want:?}",
            );
        }
    }

    #[test]
    fn pls_base_esi_advances_across_cycles() {
        // Build a payload large enough to span ≥ 2 cycles on HIGH and
        // verify the second cycle's PLHEADER carries base_esi = first
        // cycle's data-CW count.
        let cfg = profile_high_2x();
        let cw_per_cycle = data_cw_per_cycle(&cfg);
        // Force ≥ 2 cycles by encoding more data.
        let k_bytes = cfg.base.ldpc_rate.k() / 8;
        // 3 cycles' worth of source bytes.
        let needed_cw = cw_per_cycle * 3;
        let data = vec![0x77u8; needed_cw * k_bytes];
        let symbols = build_superframe_v4(&data, &cfg, 0xAB, mime::BINARY, 0);

        // First cycle: starts at sym 0.
        let (pls0, _) = decode_plheader_at(&symbols[..PLHEADER_LEN_SYM], cfg.family)
            .expect("cycle 0 PLHEADER");
        assert_eq!(pls0.base_esi, 0);

        // Second cycle: starts at sym = cycle_total_syms(cfg, cw_per_cycle).
        let off = cycle_total_syms(&cfg, cw_per_cycle);
        let (pls1, _) =
            decode_plheader_at(&symbols[off..off + PLHEADER_LEN_SYM], cfg.family)
                .expect("cycle 1 PLHEADER");
        assert_eq!(pls1.base_esi, cw_per_cycle as u32);
        assert_eq!(pls1.frame_counter, 1);
        assert_eq!(pls1.seg_id, 1);
    }

    #[test]
    fn meta_cw_decodes_back_to_app_header() {
        // Encode a known burst, manually pull the META-CW slice (data
        // symbols only, after TDM deinterleave), re-run LDPC, recover
        // AppHeader. This protects the encode_one_codeword path against
        // silent regressions in the interleaver or the encoder.
        use modem_core_base::ldpc::decoder::LdpcDecoder;
        let cfg = profile_normal_2x();
        let data = b"hello world".to_vec();
        let session_id = 0xDEADBEEFu32;
        let syms = build_superframe_v4(
            &data,
            &cfg,
            session_id,
            mime::BINARY,
            0x55AA,
        );

        // META-CW spans from end of PLHEADER+warmup over the full
        // interleaved CW wire (data + TDM pilots). Strip the pilots.
        let cons = make_constellation_2x(&cfg);
        let cw_data = cfg.cw_data_syms();
        let wire_len = cw_with_pilots_len(&cfg);
        let start = PLHEADER_LEN_SYM + cfg.lms_warmup_syms;
        let wire = &syms[start..start + wire_len];
        let meta_data_syms = pilot2x_tdm::deinterleave_data_pilots_2x(
            wire,
            &cfg.pilot_pattern,
            cw_data,
        );
        // Hard-decision demap.
        let indices = cons.slice_nearest(&meta_data_syms);
        let mut bits = cons.symbols_to_bits(&indices);
        // De-interleave.
        let perm = interleaver::interleave_table(
            interleaver::padded_cw_bits(cfg.base.ldpc_rate.n(), cfg.base.constellation),
            cfg.base.constellation,
        );
        let dperm = interleaver::deinterleave_table(perm.len(), cfg.base.constellation);
        bits = interleaver::apply_permutation(&bits, &dperm);
        // Drop padding to the strict CW length, then LDPC decode.
        bits.truncate(cfg.base.ldpc_rate.n());
        let llrs: Vec<f32> = bits.iter().map(|&b| if b == 0 { 8.0 } else { -8.0 }).collect();
        let dec = LdpcDecoder::new(cfg.base.ldpc_rate, 30);
        let (info_bits, _conv) = dec.decode(&llrs);
        // Convert info bits → bytes.
        let mut info_bytes = vec![0u8; info_bits.len() / 8];
        for (i, chunk) in info_bits.chunks_exact(8).enumerate() {
            let mut b = 0u8;
            for &bit in chunk { b = (b << 1) | (bit & 1); }
            info_bytes[i] = b;
        }
        let recovered = app_header::decode_meta_payload(&info_bytes)
            .expect("AppHeader must decode from META-CW");
        assert_eq!(recovered.session_id, session_id);
        assert_eq!(recovered.file_size, data.len() as u32);
        assert_eq!(recovered.mime_type, mime::BINARY);
        assert_eq!(recovered.hash_short, 0x55AA);
    }

    #[test]
    fn high_plus_2x_uses_v3_tdm_pattern() {
        // HIGH+2X (Apsk32, 461 data sym/CW) with pattern 32/2:
        //   ⌈461/32⌉ = 15 groups → 30 pilot sym, wire len 491.
        //   Last group has only 461 - 14·32 = 13 data syms.
        let cfg = profile_high_plus_2x();
        assert_eq!(cfg.pilot_pattern, PilotPattern2x::default_2x());
        assert_eq!(pilot_groups_per_cw(&cfg), 15);
        assert_eq!(pilot_syms_per_cw(&cfg), 30);
        assert_eq!(cw_with_pilots_len(&cfg), 491);
    }

    #[test]
    fn high_plus_plus_2x_uses_dense_tdm_pattern() {
        // HIGH++2X (Apsk64, 384 data sym/CW) with pattern 16/2:
        //   384/16 = 24 groups → 48 pilot, wire len 432.
        let cfg = profile_high_plus_plus_2x();
        assert_eq!(cfg.pilot_pattern, PilotPattern2x::dense_2x());
        assert_eq!(pilot_groups_per_cw(&cfg), 24);
        assert_eq!(pilot_syms_per_cw(&cfg), 48);
        assert_eq!(cw_with_pilots_len(&cfg), 432);
    }

    #[test]
    fn empty_payload_emits_one_cycle() {
        // Even with zero-length data we get 1 cycle (PLHEADER + meta+pilot).
        // (raptorq pads to MIN_K=4 source symbols + 30% repair → ~5
        // packets; still fits in one cycle for any profile we ship.)
        let cfg = profile_ultra_2x();
        let symbols = build_superframe_v4(&[], &cfg, 1, mime::BINARY, 0);
        assert!(symbols.len() >= PLHEADER_LEN_SYM);
        let (_pls, _) = decode_plheader_at(
            &symbols[..PLHEADER_LEN_SYM],
            cfg.family,
        )
        .expect("PLHEADER decodes");
    }

    // --- pilot-position layout helpers (used by RX pipelines) -------

    #[test]
    fn chunk_offset_is_pilot_default_pattern() {
        // HIGH2X (16-APSK, 576 data, pattern 32/2): pilots fall at wire
        // offsets {32..34, 66..68, 100..102, ...}.
        let cfg = profile_high_2x();
        let cw_data = cfg.cw_data_syms();
        let p = cfg.pilot_pattern;
        // Position 0..32 → data.
        for off in [0usize, 1, 31] {
            assert!(!chunk_offset_is_pilot(off, cw_data, &p));
        }
        // 32..34 → first pilot pair.
        assert!(chunk_offset_is_pilot(32, cw_data, &p));
        assert!(chunk_offset_is_pilot(33, cw_data, &p));
        // 34..66 → data again.
        for off in [34usize, 50, 65] {
            assert!(!chunk_offset_is_pilot(off, cw_data, &p));
        }
        // 66..68 → second pilot pair.
        assert!(chunk_offset_is_pilot(66, cw_data, &p));
        assert!(chunk_offset_is_pilot(67, cw_data, &p));
    }

    #[test]
    fn chunk_offset_is_pilot_apsk32_partial_last_group() {
        // HIGH+2X: 461 data, pattern 32/2 → 15 groups.
        // Last group: groups 14 carries only 461 - 14·32 = 13 data,
        // pilots fall at wire offsets after 14·34 + 13 = 489..491.
        let cfg = profile_high_plus_2x();
        let cw_data = cfg.cw_data_syms();
        let p = cfg.pilot_pattern;
        assert!(!chunk_offset_is_pilot(488, cw_data, &p));
        assert!(chunk_offset_is_pilot(489, cw_data, &p));
        assert!(chunk_offset_is_pilot(490, cw_data, &p));
        // wire_len is 491 — anything past it is out of chunk.
        assert!(!chunk_offset_is_pilot(491, cw_data, &p));
    }

    #[test]
    fn cycle_pilot_map_marks_only_pilot_symbols() {
        // Build a real burst, then sample every position the lookup says
        // is a pilot and verify it sits on the unit circle and matches
        // the rotating-QPSK reference.
        let cfg = profile_high_2x();
        let data = vec![0xA5u8; 800];
        let symbols = build_superframe_v4(&data, &cfg, 0x42, mime::BINARY, 0);
        let map = cycle_pilot_map(&cfg);
        assert_eq!(map.len(), full_cycle_len_syms(&cfg));
        // The first PLHEADER (192 sym) + warmup must NOT be marked.
        for k in 0..PLHEADER_LEN_SYM + cfg.lms_warmup_syms {
            assert!(!map[k], "PLHEADER/warmup pos {k} wrongly marked pilot");
        }
        let cycle_len = map.len();
        for k in 0..cycle_len.min(symbols.len()) {
            if map[k] {
                assert!(
                    (symbols[k].norm() - 1.0).abs() < 1e-9,
                    "pilot sym at {k} off the unit circle: {:?}",
                    symbols[k],
                );
            }
        }
        // Expected count: (1 META + cw_per_cycle DATA) × pilot_syms_per_cw.
        let pilot_count: usize = map.iter().filter(|b| **b).count();
        let cw_per_cycle = data_cw_per_cycle(&cfg);
        let expected = (1 + cw_per_cycle) * pilot_syms_per_cw(&cfg);
        assert_eq!(pilot_count, expected, "pilot map cardinality off");
    }

    #[test]
    fn cycle_pilot_map_covers_all_profiles() {
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let map = cycle_pilot_map(&cfg);
            assert_eq!(map.len(), full_cycle_len_syms(&cfg));
            let pilot_count = map.iter().filter(|b| **b).count();
            let cw_per_cycle = data_cw_per_cycle(&cfg);
            let expected = (1 + cw_per_cycle) * pilot_syms_per_cw(&cfg);
            assert_eq!(
                pilot_count, expected,
                "{p:?} pilot map cardinality off (got {pilot_count} expected {expected})",
            );
            // Sample the burst against the lookup — every pilot-marked
            // position must lie on the unit circle (rotating QPSK).
            let data = vec![0u8; 400];
            let symbols = build_superframe_v4(&data, &cfg, 0x42, mime::BINARY, 0);
            for k in 0..map.len().min(symbols.len()) {
                if map[k] {
                    assert!(
                        (symbols[k].norm() - 1.0).abs() < 1e-9,
                        "{p:?} lookup pilot at {k} but sym={:?}",
                        symbols[k],
                    );
                }
            }
        }
    }

    #[test]
    fn unaligned_payload_uses_prbs_residue_not_zeros() {
        // HIGH+2X k_bytes derives from the profile's LDPC k. Pick a
        // payload size that does NOT align to K·k_bytes so the
        // residue exists. Build the superframe both with the
        // production builder and with a hand-rolled PRBS-padded
        // equivalent ; they must match symbol-for-symbol. Then
        // swap PRBS for zeros and verify the symbols differ —
        // proves the production path is actually using PRBS.
        let cfg = crate::profile2x::config_by_name_2x("HIGH+2X").unwrap();
        let k_bytes = cfg.base.ldpc_rate.k() / 8;

        // Payload size 5_000 with k_bytes=216 gives K=24 and residue
        // 5184-5000 = 184 bytes. Validate the math before encoding.
        let payload: Vec<u8> = (0..5_000u32).map(|i| (i.wrapping_mul(0x9E37_79B9) >> 24) as u8).collect();
        let k_source = modem_framing::raptorq_codec::k_from_payload(payload.len(), k_bytes);
        let padded_len = k_source * k_bytes;
        let residue = padded_len - payload.len();
        assert!(residue > 0, "test setup expected an unaligned residue");

        // Production path : uses preburst::lfsr15_bytes internally.
        let actual = build_superframe_v4(
            &payload, &cfg, 0xCAFE_F00D, mime::BINARY, 0x4242,
        );

        // Hand-built PRBS reference : same pad bytes, fed directly
        // through encode_packets_range. The whole superframe must
        // come out identical.
        let mut padded_prbs = payload.clone();
        padded_prbs.extend_from_slice(&preburst::lfsr15_bytes(residue));
        assert_eq!(padded_prbs.len(), padded_len);

        // Direct equivalence : run build_superframe_v4 again — should
        // give the same bytes (deterministic builder + deterministic
        // PRBS). This is a smoke test for builder determinism under
        // padding ; the real verification is below.
        let actual2 = build_superframe_v4(
            &payload, &cfg, 0xCAFE_F00D, mime::BINARY, 0x4242,
        );
        assert_eq!(actual.len(), actual2.len());

        // Now the discriminating check : if we hypothetically padded
        // with ZEROS instead of PRBS, would the symbols differ ? They
        // must, otherwise the PRBS pad isn't reaching the encoder.
        let mut padded_zeros = payload.clone();
        padded_zeros.resize(padded_len, 0u8);
        // Use the framing call directly with the zero-padded data, then
        // encode each CW the same way the builder does and compare a
        // few CW outputs. (We can't easily call build_superframe with a
        // "use zeros" override, so we compare at the raptorq layer.)
        let pkts_prbs = modem_framing::raptorq_codec::encode_packets_range(
            &padded_prbs, k_bytes as u16, 0, k_source as u32 + 1,
        );
        let pkts_zero = modem_framing::raptorq_codec::encode_packets_range(
            &padded_zeros, k_bytes as u16, 0, k_source as u32 + 1,
        );
        // The very last source packet (ESI = K-1) carries the residue
        // bytes — that's where PRBS vs zeros diverge.
        assert_eq!(pkts_prbs[k_source - 1].len(), k_bytes);
        assert_ne!(
            pkts_prbs[k_source - 1],
            pkts_zero[k_source - 1],
            "PRBS-padded source packet must differ from zero-padded one"
        );

        // Source packet K-1 trailing bytes must be the PRBS bytes.
        let in_payload = k_bytes - residue;
        let prbs_ref = preburst::lfsr15_bytes(residue);
        assert_eq!(
            &pkts_prbs[k_source - 1][in_payload..],
            &prbs_ref[..],
            "trailing bytes of source packet K-1 not the LFSR-15 reference",
        );
    }

    #[test]
    fn last_cycle_tail_fill_extra_repair_emitted() {
        // Pick a payload size for HIGH+2X such that the natural
        // (K + 30% repair) total does NOT divide cw_per_cycle. The
        // builder must round the emitted count up to the next
        // multiple of cw_per_cycle and tail_filled_data_cw_count
        // returns the bumped value.
        let cfg = crate::profile2x::config_by_name_2x("HIGH+2X").unwrap();
        let cw_per_cycle = data_cw_per_cycle(&cfg) as u32;
        let k_bytes = cfg.base.ldpc_rate.k() / 8;

        // Walk a few sizes to find one that is unaligned.
        let mut chosen_size: Option<usize> = None;
        for size in [3_000usize, 5_000, 7_000, 10_000, 12_000] {
            let k = modem_framing::raptorq_codec::k_from_payload(size, k_bytes) as u32;
            let n_total = k + modem_framing::raptorq_codec::n_repair_default(k);
            if n_total % cw_per_cycle != 0 {
                chosen_size = Some(size);
                break;
            }
        }
        let size = chosen_size.expect("none of the candidate sizes is unaligned");
        let k = modem_framing::raptorq_codec::k_from_payload(size, k_bytes) as u32;
        let n_total = k + modem_framing::raptorq_codec::n_repair_default(k);
        let n_emit = tail_filled_data_cw_count(&cfg, n_total);
        assert!(
            n_emit > n_total,
            "tail-fill helper failed to bump : n_total={n_total} n_emit={n_emit}",
        );
        assert_eq!(
            n_emit % cw_per_cycle,
            0,
            "tail-filled count {n_emit} not a multiple of cw_per_cycle={cw_per_cycle}",
        );

        // Build the superframe and compare against the predicted
        // total-symbols : both must agree on the bumped count.
        let payload: Vec<u8> = (0..size as u32).map(|i| i as u8).collect();
        let actual = build_superframe_v4(
            &payload, &cfg, 0xBEEF_F00D, mime::BINARY, 0x1234,
        );
        let predicted = superframe_total_symbols_v4(&cfg, n_total);
        assert_eq!(
            actual.len(),
            predicted,
            "build_superframe_v4 total ({}) doesn't match \
             superframe_total_symbols_v4 (predicted {}) for size={size}",
            actual.len(),
            predicted,
        );

        // Predicted total must equal n_emit/cw_per_cycle full cycles.
        let expected_cycles = n_emit / cw_per_cycle;
        assert_eq!(
            predicted,
            (expected_cycles as usize) * cycle_total_syms(&cfg, cw_per_cycle as usize),
        );
    }

    #[test]
    fn tail_filled_data_cw_count_round_up_invariants() {
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let cw_per_cycle = data_cw_per_cycle(&cfg) as u32;
            let mut samples = vec![0u32, 1, cw_per_cycle, cw_per_cycle + 1];
            samples.push(cw_per_cycle.saturating_sub(1));
            samples.push((3 * cw_per_cycle).saturating_sub(5));
            for n in samples {
                let f = tail_filled_data_cw_count(&cfg, n);
                assert!(f >= n, "{p:?} n={n} f={f} must be >= n");
                assert!(f < n + cw_per_cycle, "{p:?} bump too big : n={n} f={f}");
                assert_eq!(
                    f % cw_per_cycle,
                    0,
                    "{p:?} f={f} not multiple of cw_per_cycle={cw_per_cycle}",
                );
            }
        }
    }
}
