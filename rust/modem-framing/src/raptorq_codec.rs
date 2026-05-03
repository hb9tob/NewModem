//! Fountain-code layer (RaptorQ, RFC 6330) on top of the LDPC codeword grid.
//!
//! Each data codeword carries exactly one RaptorQ encoded packet of `T` bytes,
//! where `T` = `k_bytes` of the current LDPC profile (108 B @ HIGH rate 3/4,
//! 144 B @ ROBUST rate 1/2, etc.). The packet's ESI is *not* embedded in the
//! payload — it is reconstructed from the linear position of the codeword in
//! the data stream (tracked by the marker's `base_esi`).
//!
//! # Rationale
//! The raptorq crate's `EncodingPacket` is a `(PayloadId, Vec<u8>)` pair.
//! `PayloadId` is a 32-bit compound of `(SBN: 8 bits, ESI: 24 bits)`. By
//! forcing a single source block (Z = 1), SBN is always 0 and ESI matches
//! the linear packet index — exactly what the V3 marker's `base_esi` already
//! carries. No extra on-the-wire overhead.
//!
//! # Minimum K
//! RaptorQ tolerates K ≥ 1 but the crate rounds up internally — we enforce
//! `K ≥ MIN_K` by zero-padding small payloads before encoding, so the
//! decoder always has a well-defined parameter set.

use std::collections::HashMap;

use raptorq::{
    Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation, PayloadId,
};

/// Default repair-symbols percentage on top of K when building the initial
/// TX burst (additional "More" bursts are sized from the GUI combo).
pub const REPAIR_PCT_DEFAULT: u32 = 30;

/// Minimum number of source symbols. Payloads that would yield K < MIN_K are
/// zero-padded in the encoder so K = MIN_K ; the RX truncates to `file_size`
/// after decoding, so padding is invisible to the user.
pub const MIN_K: usize = 4;

/// Compute the logical K (source-symbol count) for a given payload and T.
/// MIN_K is enforced even for tiny payloads.
pub fn k_from_payload(file_size: usize, t_bytes: usize) -> usize {
    let raw = (file_size + t_bytes - 1) / t_bytes.max(1);
    raw.max(MIN_K)
}

/// Build OTI (ObjectTransmissionInformation) for a payload of `file_size`
/// bytes encoded in `T`-byte symbols, forcing a single source block so that
/// ESI = linear packet index.
pub fn build_oti(file_size: u32, t_bytes: u16) -> ObjectTransmissionInformation {
    // With K < 2^16 (our realistic max ~ 10k), the defaults yield Z = 1 and
    // N = 1. We keep the constructor generic here so that higher-level code
    // doesn't depend on the inner packet structure.
    let padded_size = (k_from_payload(file_size as usize, t_bytes as usize)
        * t_bytes as usize) as u64;
    ObjectTransmissionInformation::with_defaults(padded_size, t_bytes)
}

/// Encode `data` into `K + n_repair` packets of exactly `t_bytes` bytes each.
///
/// Returns the packets in emission order (ESI 0 .. K + n_repair − 1).
/// `file_size` is the *true* payload length ; the encoder zero-pads up to
/// `K * T` for K < MIN_K or non-multiple payloads, the RX truncates at the
/// end using the AppHeader.
pub fn encode_packets(
    data: &[u8],
    t_bytes: u16,
    repair_pct: u32,
) -> Vec<Vec<u8>> {
    let k = k_from_payload(data.len(), t_bytes as usize) as u32;
    let n_repair = (k * repair_pct) / 100;
    encode_packets_range(data, t_bytes, 0, k + n_repair)
}

/// Encode `data` and return exactly the packets whose ESI fall in
/// `[esi_start, esi_start + count)`.
///
/// Used to generate additional bursts after an initial TX : the operator
/// clicks "More" with a target percentage, the caller computes `count =
/// K * pct / 100` and passes `esi_start = esi_max_already_sent + 1`.
///
/// Both the initial and subsequent bursts must use the *same* `data` and
/// `t_bytes` so that the RaptorQ source block stays identical ; any change
/// → new session_id.
pub fn encode_packets_range(
    data: &[u8],
    t_bytes: u16,
    esi_start: u32,
    count: u32,
) -> Vec<Vec<u8>> {
    if count == 0 {
        return Vec::new();
    }
    let t = t_bytes as usize;
    let k = k_from_payload(data.len(), t) as u32;

    // Pad to exactly K·T so the encoder works on aligned input.
    let mut padded = data.to_vec();
    padded.resize((k as usize) * t, 0u8);

    let oti = ObjectTransmissionInformation::with_defaults(padded.len() as u64, t_bytes);
    let encoder = Encoder::new(&padded, oti);

    // The raptorq crate produces K source packets + any number of repair
    // packets. To obtain packets at arbitrary ESIs, we ask for enough repair
    // so that the last required ESI is included, then filter.
    let esi_end = esi_start + count; // exclusive
    let n_repair_needed = esi_end.saturating_sub(k);
    let all_packets = encoder.get_encoded_packets(n_repair_needed);

    all_packets
        .into_iter()
        .skip(esi_start as usize)
        .take(count as usize)
        .map(|p| p.data().to_vec())
        .collect()
}

/// Convenience : number of repair packets to emit for an initial burst.
pub fn n_repair_default(k: u32) -> u32 {
    (k * REPAIR_PCT_DEFAULT) / 100
}

/// Attempt to decode a payload from an ESI → bytes map.
/// Returns `Some(data)` of exactly `file_size` bytes on success, `None` if
/// not enough packets are available yet.
pub fn try_decode(
    packets: &HashMap<u32, Vec<u8>>,
    file_size: u32,
    t_bytes: u16,
) -> Option<Vec<u8>> {
    if packets.is_empty() {
        return None;
    }
    let oti = build_oti(file_size, t_bytes);
    let mut decoder = Decoder::new(oti);
    for (&esi, bytes) in packets.iter() {
        // Single source block → SBN = 0, ESI = linear index.
        let pid = PayloadId::new(0, esi);
        let result = decoder.decode(EncodingPacket::new(pid, bytes.clone()));
        if let Some(mut payload) = result {
            payload.truncate(file_size as usize);
            return Some(payload);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let data = b"Hello RaptorQ !".to_vec();
        let t: u16 = 16;
        let packets = encode_packets(&data, t, REPAIR_PCT_DEFAULT);
        // K = max(MIN_K, ceil(15/16)) = 4, repair = 4 * 30 / 100 = 1, total ≥ 5
        assert!(packets.len() >= 5);

        let mut map = HashMap::new();
        for (esi, p) in packets.iter().enumerate() {
            map.insert(esi as u32, p.clone());
        }
        let decoded = try_decode(&map, data.len() as u32, t).expect("should decode");
        assert_eq!(decoded, data);
    }

    #[test]
    fn roundtrip_drops_10pct() {
        let data: Vec<u8> = (0..5_000).map(|i| (i as u32).wrapping_mul(2654435761) as u8).collect();
        let t: u16 = 108;
        let packets = encode_packets(&data, t, REPAIR_PCT_DEFAULT);
        // Drop every 10th packet.
        let mut map = HashMap::new();
        for (esi, p) in packets.iter().enumerate() {
            if esi % 10 == 0 {
                continue;
            }
            map.insert(esi as u32, p.clone());
        }
        let decoded = try_decode(&map, data.len() as u32, t).expect("should decode w/ 10% drops");
        assert_eq!(decoded, data);
    }

    #[test]
    fn too_few_packets_returns_none() {
        let data: Vec<u8> = (0..1000).map(|i| i as u8).collect();
        let t: u16 = 108;
        let packets = encode_packets(&data, t, REPAIR_PCT_DEFAULT);
        // Keep only 2 packets — far below K.
        let mut map = HashMap::new();
        for (esi, p) in packets.iter().take(2).enumerate() {
            map.insert(esi as u32, p.clone());
        }
        assert!(try_decode(&map, data.len() as u32, t).is_none());
    }

    /// Multi-burst scenario : initial burst covers ESI 0..K+30% ; a second
    /// "More" burst adds packets at ESI K+30%..K+50%. The receiver should
    /// decode from the union.
    #[test]
    fn roundtrip_two_bursts_union() {
        let data: Vec<u8> = (0..3000).map(|i| ((i * 37) ^ 0xA5) as u8).collect();
        let t: u16 = 108;
        let k = k_from_payload(data.len(), t as usize) as u32;

        // Burst 1 : packets 0..K + K*30%
        let b1 = encode_packets(&data, t, REPAIR_PCT_DEFAULT);
        let b1_end = k + n_repair_default(k);
        assert_eq!(b1.len() as u32, b1_end);

        // Burst 2 : 20% more, starting right after burst 1.
        let n_more = (k * 20) / 100;
        let b2 = encode_packets_range(&data, t, b1_end, n_more);
        assert_eq!(b2.len() as u32, n_more);

        // Simulate loss : drop 20 % of burst 1, then recover via burst 2.
        // Union must still exceed K packets so the fountain converges.
        let mut map = HashMap::new();
        let skip = (b1.len() * 20) / 100;
        for (i, p) in b1.iter().enumerate().skip(skip) {
            map.insert(i as u32, p.clone());
        }
        for (j, p) in b2.iter().enumerate() {
            map.insert(b1_end + j as u32, p.clone());
        }
        assert!(map.len() as u32 >= k, "test setup must provide ≥ K packets");
        let decoded = try_decode(&map, data.len() as u32, t)
            .expect("burst union should decode");
        assert_eq!(decoded, data);
    }

    #[test]
    fn encode_range_is_deterministic() {
        let data: Vec<u8> = (0..500).map(|i| i as u8).collect();
        let t: u16 = 32;
        let a = encode_packets_range(&data, t, 20, 5);
        let b = encode_packets_range(&data, t, 20, 5);
        assert_eq!(a, b, "same input → same packets at a given ESI");
    }
}
