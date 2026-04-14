//! Reed-Solomon en mode erasure (Vandermonde / GF(2^8)).
//!
//! Applique au niveau du BLOC (90 octets par defaut), pas du byte. Chaque
//! bloc est un "shard" Reed-Solomon. Le LDPC en couche basse signale les
//! blocs cassés (echec de CRC per-bloc), et le RS recupere les blocs
//! manquants.
//!
//! Contrainte GF(2^8) : K + M <= 256. Pour un payload plus gros il faut
//! decouper en plusieurs frames.
//!
//! 4 niveaux :
//!   0 : aucun RS
//!   1 : M = ceil(K/16)  (~6 % overhead, 1 bloc/16 tolere)
//!   2 : M = ceil(K/8)   (~12 %, 2 blocs/16)
//!   3 : M = ceil(K/4)   (~25 %, 4 blocs/16)

use reed_solomon_erasure::galois_8::ReedSolomon;

/// Nombre de blocs parity pour K blocs data au niveau donne.
pub fn parity_blocks_for(k: usize, rs_level: u8) -> usize {
    if k == 0 {
        return 0;
    }
    match rs_level {
        0 => 0,
        1 => (k + 15) / 16,
        2 => (k + 7) / 8,
        3 => (k + 3) / 4,
        _ => 0,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RsError {
    #[error("RS level inconnu: {0}")]
    UnknownLevel(u8),
    #[error("K+M trop grand pour GF(2^8): K={0}, M={1}")]
    TooManyShards(usize, usize),
    #[error("RS erasure error: {0}")]
    Rse(String),
    #[error("nombre de blocs ne correspond pas: attendu {0}, recu {1}")]
    ShardCountMismatch(usize, usize),
    #[error("trop de blocs perdus pour recuperer (K={0}, perdus={1}, M={2})")]
    TooManyLosses(usize, usize, usize),
}

/// Encode les blocs data avec RS. Retourne (data + parity) concatenes.
/// Tous les blocs doivent avoir la meme taille (typiquement BLOCK_BYTES=90).
pub fn encode(data_blocks: &[Vec<u8>], rs_level: u8) -> Result<Vec<Vec<u8>>, RsError> {
    if rs_level == 0 {
        return Ok(data_blocks.to_vec());
    }
    if rs_level > 3 {
        return Err(RsError::UnknownLevel(rs_level));
    }
    let k = data_blocks.len();
    let m = parity_blocks_for(k, rs_level);
    if k + m > 256 {
        return Err(RsError::TooManyShards(k, m));
    }
    let rs = ReedSolomon::new(k, m)
        .map_err(|e| RsError::Rse(format!("{:?}", e)))?;

    // Prepare shards : K data + M parity vides
    let shard_size = data_blocks[0].len();
    let mut shards: Vec<Vec<u8>> = data_blocks.iter().cloned().collect();
    for _ in 0..m {
        shards.push(vec![0u8; shard_size]);
    }
    rs.encode(&mut shards)
        .map_err(|e| RsError::Rse(format!("{:?}", e)))?;
    Ok(shards)
}

/// Decode en mode erasure : recupere les blocs data manquants.
///
/// `shards` est un Vec<Option<Vec<u8>>> de taille K+M ou None = bloc perdu.
/// Retourne les K blocs data recuperes, ou erreur si recovery impossible.
pub fn decode(
    mut shards: Vec<Option<Vec<u8>>>,
    k: usize,
    m: usize,
) -> Result<Vec<Vec<u8>>, RsError> {
    if m == 0 {
        // Pas de RS : tous les blocs data doivent etre presents
        if shards.len() < k {
            return Err(RsError::ShardCountMismatch(k, shards.len()));
        }
        let mut out = Vec::with_capacity(k);
        for s in shards.into_iter().take(k) {
            out.push(s.ok_or(RsError::TooManyLosses(k, 1, 0))?);
        }
        return Ok(out);
    }
    if shards.len() != k + m {
        return Err(RsError::ShardCountMismatch(k + m, shards.len()));
    }
    let lost = shards.iter().filter(|s| s.is_none()).count();
    if lost > m {
        return Err(RsError::TooManyLosses(k, lost, m));
    }
    let rs = ReedSolomon::new(k, m)
        .map_err(|e| RsError::Rse(format!("{:?}", e)))?;
    rs.reconstruct_data(&mut shards)
        .map_err(|e| RsError::Rse(format!("{:?}", e)))?;
    let mut out = Vec::with_capacity(k);
    for s in shards.into_iter().take(k) {
        out.push(s.ok_or(RsError::TooManyLosses(k, 1, m))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_counts() {
        assert_eq!(parity_blocks_for(16, 1), 1);
        assert_eq!(parity_blocks_for(16, 2), 2);
        assert_eq!(parity_blocks_for(16, 3), 4);
        assert_eq!(parity_blocks_for(100, 3), 25);
        assert_eq!(parity_blocks_for(0, 3), 0);
        assert_eq!(parity_blocks_for(10, 0), 0);
    }

    #[test]
    fn encode_level_0_passthrough() {
        let data: Vec<Vec<u8>> = (0..5).map(|i| vec![i as u8; 10]).collect();
        let out = encode(&data, 0).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn encode_decode_roundtrip_no_loss() {
        let data: Vec<Vec<u8>> = (0..16).map(|i| vec![i as u8; 90]).collect();
        let shards = encode(&data, 2).unwrap();
        assert_eq!(shards.len(), 16 + 2);
        let opts: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        let recovered = decode(opts, 16, 2).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn decode_recovers_lost_blocks() {
        let data: Vec<Vec<u8>> = (0..16).map(|i| vec![i as u8; 90]).collect();
        let shards = encode(&data, 3).unwrap();  // M=4
        let mut opts: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        // On perd 3 blocs (level 3 tolere 4)
        opts[2] = None;
        opts[7] = None;
        opts[15] = None;
        let recovered = decode(opts, 16, 4).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn decode_fails_too_many_losses() {
        let data: Vec<Vec<u8>> = (0..16).map(|i| vec![i as u8; 90]).collect();
        let shards = encode(&data, 1).unwrap();  // M=1
        let mut opts: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        opts[0] = None;
        opts[1] = None;  // 2 perdus, max 1
        assert!(decode(opts, 16, 1).is_err());
    }

    #[test]
    fn encode_rejects_too_big() {
        let data: Vec<Vec<u8>> = (0..250).map(|_| vec![0u8; 90]).collect();
        assert!(encode(&data, 3).is_err());  // 250 + 63 > 256
    }
}
