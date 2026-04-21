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
    let t = t_bytes as usize;
    let k = k_from_payload(data.len(), t);

    // Pad to exactly K·T so the encoder works on aligned input.
    let mut padded = data.to_vec();
    padded.resize(k * t, 0u8);

    let oti = ObjectTransmissionInformation::with_defaults(padded.len() as u64, t_bytes);
    let encoder = Encoder::new(&padded, oti);

    let n_repair = ((k as u32) * repair_pct) / 100;
    let packets = encoder.get_encoded_packets(n_repair);
    packets.into_iter().map(|p| p.data().to_vec()).collect()
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
}
