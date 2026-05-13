//! Superframe assembly for the 2x wire format V4.
//!
//! Cycle layout (one PLHEADER period ≈ 4 s):
//!
//! ```text
//! [PLHEADER 192]
//!   [LMS warmup 32/64]      (only when constellation = APSK-32 or APSK-64)
//! [META-CW + pilot_block]   (LDPC codeword carrying AppHeader×4)
//! [DATA-CW[0] + pilot_block]
//! [DATA-CW[1] + pilot_block]
//! ...
//! [DATA-CW[k] + pilot_block]
//! ```
//!
//! For HighPlus2x / HighPlusPlus2x / HighPlusFiveSix2x the pilot blocks are
//! densified to 2 per CW (data half / pilot / data half / pilot) — see
//! [`crate::pilot_block::interleave_pilot_blocks`].
//!
//! Differences vs V3 frame:
//!
//! - No marker between segments (the implicit boundary is the pilot block
//!   that follows every CW; the new PLHEADER cycle is anchored on the SOF).
//! - No TDM intra-CW pilots — sparse blocks only.
//! - No runout symbols — the last pilot block already gives the RX a clean
//!   reference; a fresh PLHEADER is the only re-anchor.
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
use modem_core_base::types::Complex64;
use modem_framing::app_header::{self, AppHeader};

use crate::pilot_block::{self, PILOT_BLOCK_LEN};
use crate::plheader::{self, PlsPayload};
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

/// Symbols emitted per LDPC codeword *after* pilot interleaving.
fn cw_with_pilots_len(cfg: &ModemConfig2x) -> usize {
    cfg.cw_data_syms() + cfg.pilot_blocks_per_cw * PILOT_BLOCK_LEN
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
/// burst of `n_data_cw` data codewords. Mirror of the builder logic so
/// the worker / TX duration estimator stays in sync without resimulating
/// the whole encode.
pub fn superframe_total_symbols_v4(cfg: &ModemConfig2x, n_data_cw: u32) -> usize {
    let cw_per_cycle = data_cw_per_cycle(cfg);
    let n_data_cw = n_data_cw as usize;
    let n_full_cycles = n_data_cw / cw_per_cycle;
    let leftover = n_data_cw - n_full_cycles * cw_per_cycle;
    let mut total = n_full_cycles * cycle_total_syms(cfg, cw_per_cycle);
    if leftover > 0 || n_full_cycles == 0 {
        // A trailing partial cycle (or the whole burst when n_data_cw <
        // cw_per_cycle) still emits its own PLHEADER + warmup + META.
        total += cycle_total_syms(cfg, leftover);
    }
    total
}

/// Symbol count for an EOT frame: a single PLHEADER cycle carrying the
/// META-CW (zero-data AppHeader) and zero data codewords.
pub fn eot_frame_symbols_v4(cfg: &ModemConfig2x) -> usize {
    cycle_total_syms(cfg, 0)
}

/// True when `chunk_offset` (within a `cw_with_pilots_len(cfg)`-sized
/// CW chunk) lands inside a pilot block. Mirror of the encoder's
/// [`pilot_block::interleave_pilot_blocks`] split logic, exposed here for
/// receive-side machinery (timing recovery) that wants to know which
/// emitted symbols are unit-magnitude pilots without having to drive the
/// full deinterleaver.
pub fn chunk_offset_is_pilot(
    chunk_offset: usize,
    cw_data_syms: usize,
    pilot_blocks_per_cw: usize,
) -> bool {
    if pilot_blocks_per_cw == 0 {
        return false;
    }
    let base_chunk = cw_data_syms / pilot_blocks_per_cw;
    let mut data_cursor = 0usize;
    let mut wire_cursor = 0usize;
    for k in 0..pilot_blocks_per_cw {
        let take = if k + 1 == pilot_blocks_per_cw {
            cw_data_syms - data_cursor
        } else {
            base_chunk
        };
        wire_cursor += take;
        if chunk_offset >= wire_cursor && chunk_offset < wire_cursor + PILOT_BLOCK_LEN {
            return true;
        }
        wire_cursor += PILOT_BLOCK_LEN;
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

/// Pre-compute a per-cycle pilot-position map: `map[k] = true` iff symbol
/// offset `k` within a full PLHEADER cycle is a pilot symbol (one of the
/// 36-sym `(1+j)/√2` blocks). The PLHEADER (192 sym SOF+PLS), LMS warmup
/// and every data symbol map to `false`.
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
    // META-CW + cw_per_cycle DATA-CWs all share the same chunk layout.
    for cw_idx in 0..=cw_per_cycle {
        let chunk_start = cw_block_start + cw_idx * cw_with_pilots;
        for off in 0..cw_with_pilots {
            if chunk_offset_is_pilot(off, cfg.cw_data_syms(), cfg.pilot_blocks_per_cw) {
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

    // Source/repair packets for this burst — same range as V3.
    let packets = modem_framing::raptorq_codec::encode_packets_range(
        data,
        k_bytes as u16,
        esi_start,
        n_packets,
    );
    let k_source = modem_framing::raptorq_codec::k_from_payload(data.len(), k_bytes);

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

        // META-CW + its pilot block(s).
        let (meta_w_pilot, _pos) =
            pilot_block::interleave_pilot_blocks(&meta_cw_syms, cfg.pilot_blocks_per_cw);
        out.extend(meta_w_pilot);

        // Data CWs of this cycle.
        let cw_take = cw_per_cycle.min(n_data_cw.saturating_sub(data_cursor));
        for k in 0..cw_take {
            let (cw_w_pilot, _pos) = pilot_block::interleave_pilot_blocks(
                &data_cw_syms[data_cursor + k],
                cfg.pilot_blocks_per_cw,
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
    let (meta_w_pilot, _pos) =
        pilot_block::interleave_pilot_blocks(&meta_cw_syms, cfg.pilot_blocks_per_cw);
    out.extend(meta_w_pilot);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // ULTRA at 500 Bd: cycle ~4 s = 2000 sym. PLHEADER 192 + meta CW
        // (1152+36=1188) = 1380 → 620 budget / 1188 per CW → 0 → clamped to 1.
        // ROBUST at 1000 Bd: 4000 sym budget; PLHEADER+meta+0 warmup =
        // 192+1188 = 1380 → 2620 / 1188 = 2 data CW.
        // HIGH at 1500 Bd: 6000 sym; PLHEADER + meta(576+36) = 192+612 = 804
        //   → 5196 / 612 = 8 data CW.
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
    fn data_cw_pilot_block_sequence() {
        // After the PLHEADER + warmup + META-CW the layout is:
        //   pilot_block, then DATA-CW[0] data, then pilot_block, ...
        // Since the META-CW interleaver puts pilot blocks at the END of
        // each chunk, the symbol immediately after warmup is the first
        // data symbol of the META-CW (not a pilot). Past meta_with_pilot
        // we should land on a data sym of CW[0].
        let cfg = profile_high_2x(); // 16-APSK, 1 block/CW
        let data = vec![0u8; 200];
        let syms = build_superframe_v4(&data, &cfg, 1, mime::BINARY, 0);
        let cw_data = cfg.cw_data_syms();
        let p = PILOT_BLOCK_LEN;
        // Index of the start of the first pilot block (end of META-CW).
        let pilot_start = PLHEADER_LEN_SYM + cfg.lms_warmup_syms + cw_data;
        let pilot_end = pilot_start + p;
        // All pilot symbols equal PILOT_SYMBOL.
        for k in pilot_start..pilot_end {
            assert!(
                (syms[k] - pilot_block::PILOT_SYMBOL).norm() < 1e-12,
                "expected pilot at index {k}",
            );
        }
        // Symbol at pilot_end must be a data symbol of DATA-CW[0]: real or
        // imag != ±1/√2 (would be a pilot collision) — for 16-APSK no data
        // point sits exactly at PILOT_SYMBOL.
        let s = syms[pilot_end];
        assert!(
            (s - pilot_block::PILOT_SYMBOL).norm() > 1e-6,
            "first data sym after pilot should not be a pilot value: {s}",
        );
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
        // Encode a known burst, manually pull the META-CW slice (no pilots),
        // re-run LDPC, recover AppHeader. This protects the encode_one_codeword
        // path against silent regressions in the interleaver or the encoder.
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

        // META-CW spans from end of PLHEADER+warmup to + cw_data_syms.
        let cons = make_constellation_2x(&cfg);
        let cw_data = cfg.cw_data_syms();
        let start = PLHEADER_LEN_SYM + cfg.lms_warmup_syms;
        let meta_data_syms = &syms[start..start + cw_data];
        // Hard-decision demap.
        let indices = cons.slice_nearest(meta_data_syms);
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
    fn high_plus_2x_uses_two_pilot_blocks_per_cw() {
        // HIGH+2X (Apsk32, 461 data sym/CW, 2 pilot blocks/CW):
        //   META-CW first chunk (floor 461/2 = 230) | pilot | second chunk
        //   (461-230 = 231) | pilot.
        let cfg = profile_high_plus_2x();
        assert_eq!(cfg.pilot_blocks_per_cw, 2);
        let data = vec![0u8; 200];
        let syms = build_superframe_v4(&data, &cfg, 1, mime::BINARY, 0);
        let cw_data = cfg.cw_data_syms();
        let warmup = cfg.lms_warmup_syms;
        let chunk_sz = cw_data / 2; // 230
        // First pilot block: at sym = PLHEADER + warmup + chunk_sz.
        let p1_start = PLHEADER_LEN_SYM + warmup + chunk_sz;
        for k in 0..PILOT_BLOCK_LEN {
            assert!(
                (syms[p1_start + k] - pilot_block::PILOT_SYMBOL).norm() < 1e-12,
                "first pilot block of META-CW corrupted at +{k}",
            );
        }
        // Second pilot block: skip first pilot + remaining chunk (cw_data
        // - chunk_sz = 231 sym).
        let p2_start = p1_start + PILOT_BLOCK_LEN + (cw_data - chunk_sz);
        for k in 0..PILOT_BLOCK_LEN {
            assert!(
                (syms[p2_start + k] - pilot_block::PILOT_SYMBOL).norm() < 1e-12,
                "second pilot block of META-CW corrupted at +{k}",
            );
        }
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

    // --- pilot-position layout helpers (used by StreamingFrontend) -------

    #[test]
    fn chunk_offset_is_pilot_one_block_per_cw() {
        // HIGH2X: 1 pilot block at the end of each cw_data_syms chunk.
        let cfg = profile_high_2x();
        let cw_data = cfg.cw_data_syms();
        // Data region [0, cw_data) → never pilot.
        for off in [0usize, 1, cw_data / 2, cw_data - 1] {
            assert!(!chunk_offset_is_pilot(off, cw_data, cfg.pilot_blocks_per_cw));
        }
        // Pilot region [cw_data, cw_data + 36) → all pilot.
        for off in cw_data..cw_data + PILOT_BLOCK_LEN {
            assert!(chunk_offset_is_pilot(off, cw_data, cfg.pilot_blocks_per_cw));
        }
        // Past pilot → false (the lookup is for ONE chunk; the caller
        // walks chunks separately).
        assert!(!chunk_offset_is_pilot(
            cw_data + PILOT_BLOCK_LEN,
            cw_data,
            cfg.pilot_blocks_per_cw,
        ));
    }

    #[test]
    fn chunk_offset_is_pilot_two_blocks_apsk32_uneven() {
        // Apsk32 padded CW = 461 data sym + 2 pilot blocks → 230 / 36 / 231 / 36.
        let cw_data = 461;
        let blocks = 2;
        // First data half [0, 230) → data.
        for &off in &[0usize, 100, 229] {
            assert!(!chunk_offset_is_pilot(off, cw_data, blocks));
        }
        // First pilot [230, 266) → pilot.
        for off in 230..266 {
            assert!(chunk_offset_is_pilot(off, cw_data, blocks));
        }
        // Second data chunk [266, 497) → data.
        for &off in &[266usize, 400, 496] {
            assert!(!chunk_offset_is_pilot(off, cw_data, blocks));
        }
        // Second pilot [497, 533) → pilot.
        for off in 497..533 {
            assert!(chunk_offset_is_pilot(off, cw_data, blocks));
        }
    }

    #[test]
    fn cycle_pilot_map_marks_only_pilot_symbols() {
        // Build a real burst, then sample every position the lookup says
        // is a pilot and verify it equals PILOT_SYMBOL.
        let cfg = profile_high_2x();
        let data = vec![0xA5u8; 800];
        let symbols = build_superframe_v4(&data, &cfg, 0x42, mime::BINARY, 0);
        let map = cycle_pilot_map(&cfg);
        assert_eq!(map.len(), full_cycle_len_syms(&cfg));
        // The first PLHEADER (192 sym) + warmup must NOT be marked.
        for k in 0..PLHEADER_LEN_SYM + cfg.lms_warmup_syms {
            assert!(!map[k], "PLHEADER/warmup pos {k} wrongly marked pilot");
        }
        // Every position marked as pilot must hold exactly PILOT_SYMBOL
        // in the first cycle of the actual modulated burst.
        let cycle_len = map.len();
        for k in 0..cycle_len.min(symbols.len()) {
            if map[k] {
                assert!(
                    (symbols[k] - pilot_block::PILOT_SYMBOL).norm() < 1e-12,
                    "lookup says pilot at {k} but sym is {:?}",
                    symbols[k],
                );
            }
        }
        // And the FRACTION of pilot positions matches the expected
        // pilot_blocks_per_cw · (1 + cw_per_cycle) · 36 / cycle_len.
        let pilot_count: usize = map.iter().filter(|b| **b).count();
        let cw_per_cycle = data_cw_per_cycle(&cfg);
        let expected = cfg.pilot_blocks_per_cw * (1 + cw_per_cycle) * PILOT_BLOCK_LEN;
        assert_eq!(pilot_count, expected, "pilot map cardinality off");
    }

    #[test]
    fn cycle_pilot_map_covers_all_profiles() {
        // Sanity: every shipped profile produces a non-empty pilot map
        // with the correct cardinality and matches the burst content.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let map = cycle_pilot_map(&cfg);
            assert_eq!(map.len(), full_cycle_len_syms(&cfg));
            let pilot_count = map.iter().filter(|b| **b).count();
            let cw_per_cycle = data_cw_per_cycle(&cfg);
            let expected = cfg.pilot_blocks_per_cw * (1 + cw_per_cycle) * PILOT_BLOCK_LEN;
            assert_eq!(
                pilot_count, expected,
                "{p:?} pilot map cardinality off (got {pilot_count} expected {expected})",
            );
            // Sample the burst against the lookup — every pilot-marked
            // position must equal PILOT_SYMBOL.
            let data = vec![0u8; 400];
            let symbols = build_superframe_v4(&data, &cfg, 0x42, mime::BINARY, 0);
            for k in 0..map.len().min(symbols.len()) {
                if map[k] {
                    assert!(
                        (symbols[k] - pilot_block::PILOT_SYMBOL).norm() < 1e-12,
                        "{p:?} lookup pilot at {k} but sym={:?}",
                        symbols[k],
                    );
                }
            }
        }
    }
}
