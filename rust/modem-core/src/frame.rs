//! Assemblage et desassemblage d'une frame complete.
//!
//! Une frame = [Header 16 B | Body encode (+ RS si applicable)].
//!
//! Le body interne contient :
//!   filename_length (1 octet)
//!   filename (UTF-8, N octets)
//!   payload (M octets)
//!   body_crc32 (4 octets BE)
//!
//! Le body est decoupe en blocs de BLOCK_BYTES = 90 octets (1 codeword LDPC
//! info). Le dernier bloc est padde avec des zeros si necessaire.
//!
//! RS n'est pas encore implemente (rs_level=0 uniquement pour l'instant).

use crate::crc::crc32;
use crate::header::{FrameHeader, FLAG_LAST};
use crate::rs::{encode as rs_encode, parity_blocks_for, RsError};
use crate::ModemMode;

/// Taille d'un bloc de data = 1 codeword LDPC info (valable pour rate 1/2 et 3/4).
pub const BLOCK_BYTES: usize = 90;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("filename trop long (max 255 octets UTF-8), recu {0}")]
    FilenameTooLong(usize),
    #[error("body corrompu : CRC32 mismatch")]
    BadBodyCrc,
    #[error("frame trop courte")]
    Truncated,
    #[error("RS: {0}")]
    Rs(#[from] RsError),
}

/// Construit le body a partir de (filename, payload).
pub fn build_body(filename: &str, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let fn_bytes = filename.as_bytes();
    if fn_bytes.len() > 255 {
        return Err(FrameError::FilenameTooLong(fn_bytes.len()));
    }
    let mut body = Vec::with_capacity(1 + fn_bytes.len() + payload.len() + 4);
    body.push(fn_bytes.len() as u8);
    body.extend_from_slice(fn_bytes);
    body.extend_from_slice(payload);
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    Ok(body)
}

/// Extrait (filename, payload) d'un body decode. Verifie le CRC32.
pub fn parse_body(body: &[u8]) -> Result<(String, Vec<u8>), FrameError> {
    if body.len() < 5 {
        return Err(FrameError::Truncated);
    }
    let crc_pos = body.len() - 4;
    let crc_recv = u32::from_be_bytes([
        body[crc_pos], body[crc_pos + 1], body[crc_pos + 2], body[crc_pos + 3],
    ]);
    let crc_calc = crc32(&body[..crc_pos]);
    if crc_recv != crc_calc {
        return Err(FrameError::BadBodyCrc);
    }
    let fn_len = body[0] as usize;
    if body.len() < 1 + fn_len + 4 {
        return Err(FrameError::Truncated);
    }
    let filename = String::from_utf8_lossy(&body[1..1 + fn_len]).to_string();
    let payload = body[1 + fn_len..crc_pos].to_vec();
    Ok((filename, payload))
}

/// Divise le body en blocs de BLOCK_BYTES. Derniere bloc padde a zero.
pub fn split_into_blocks(body: &[u8]) -> Vec<Vec<u8>> {
    let n_blocks = (body.len() + BLOCK_BYTES - 1) / BLOCK_BYTES;
    let mut blocks = Vec::with_capacity(n_blocks);
    for i in 0..n_blocks {
        let start = i * BLOCK_BYTES;
        let end = (start + BLOCK_BYTES).min(body.len());
        let mut block = vec![0u8; BLOCK_BYTES];
        block[..end - start].copy_from_slice(&body[start..end]);
        blocks.push(block);
    }
    blocks
}

/// Recompose le body a partir de blocs decodes. Le body a sa propre longueur
/// indiquee par le champ interne, mais ici on ne la connait pas : le caller
/// doit trimmer via parse_body qui lit filename_len pour connaitre la vraie
/// taille, puis le CRC32 est aux 4 dernieres octets.
pub fn concatenate_blocks(blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(blocks.len() * BLOCK_BYTES);
    for b in blocks {
        out.extend_from_slice(b);
    }
    out
}

/// Assemble la frame complete : header + blocs data + blocs parity RS.
///
/// Retourne les bytes de la frame, prets a passer dans le modulateur.
pub fn build_frame(
    mode: ModemMode,
    rs_level: u8,
    frame_id: u16,
    is_last: bool,
    filename: &str,
    payload: &[u8],
) -> Result<Vec<u8>, FrameError> {
    let body = build_body(filename, payload)?;
    let data_blocks = split_into_blocks(&body);
    let n_data = data_blocks.len();
    let n_parity = parity_blocks_for(n_data, rs_level);

    // RS encode : retourne data + parity blocs
    let all_blocks = rs_encode(&data_blocks, rs_level)?;
    assert_eq!(all_blocks.len(), n_data + n_parity);

    let header = FrameHeader {
        version: crate::header::VERSION,
        mode_code: mode.to_code(),
        rs_level,
        n_data_blocks: n_data as u16,
        n_parity_blocks: n_parity as u16,
        frame_id,
        flags: if is_last { FLAG_LAST } else { 0 },
    };

    let header_bytes = header.encode();
    let mut out = Vec::with_capacity(header_bytes.len() + all_blocks.len() * BLOCK_BYTES);
    out.extend_from_slice(&header_bytes);
    for block in &all_blocks {
        out.extend_from_slice(block);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_roundtrip() {
        let fn_ = "image.png";
        let payload = b"hello world, this is payload data.";
        let body = build_body(fn_, payload).unwrap();
        let (fn_back, pl_back) = parse_body(&body).unwrap();
        assert_eq!(fn_, fn_back);
        assert_eq!(payload, &pl_back[..]);
    }

    #[test]
    fn body_filename_too_long() {
        let long = "a".repeat(300);
        assert!(build_body(&long, b"x").is_err());
    }

    #[test]
    fn body_empty_payload() {
        let body = build_body("x.txt", b"").unwrap();
        let (f, p) = parse_body(&body).unwrap();
        assert_eq!(f, "x.txt");
        assert!(p.is_empty());
    }

    #[test]
    fn split_and_join() {
        let body = vec![1u8, 2, 3, 4, 5].into_iter().cycle().take(200).collect::<Vec<_>>();
        let blocks = split_into_blocks(&body);
        assert_eq!(blocks.len(), 3);  // ceil(200/90) = 3
        assert_eq!(blocks[0].len(), BLOCK_BYTES);
        let joined = concatenate_blocks(&blocks);
        assert_eq!(joined.len(), 3 * BLOCK_BYTES);
        assert_eq!(&joined[..200], &body[..]);
        // Derniere partie est du padding
        for &b in &joined[200..] {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn build_frame_basic() {
        let bytes = build_frame(
            ModemMode::Qam16R34_1500, 0, 42, true, "test.bin", &[0xAA; 100],
        ).unwrap();
        // 16 header + ceil((1+8+100+4)/90)*90 = 16 + 2*90 = 196
        assert_eq!(bytes.len(), 16 + 2 * BLOCK_BYTES);
        // Le header se decode
        let h = FrameHeader::decode(&bytes[..16]).unwrap();
        assert_eq!(h.frame_id, 42);
        assert_eq!(h.n_data_blocks, 2);
        assert!(h.is_last_frame());
    }

    #[test]
    fn build_frame_with_rs() {
        let bytes = build_frame(
            ModemMode::Qam16R34_1500, 2, 7, false, "test.bin", &[0xAA; 1000],
        ).unwrap();
        let h = FrameHeader::decode(&bytes[..16]).unwrap();
        // body = 1 + 8 + 1000 + 4 = 1013 bytes
        // blocs = ceil(1013/90) = 12 data blocs
        // parity level 2 = ceil(12/8) = 2
        assert_eq!(h.n_data_blocks, 12);
        assert_eq!(h.n_parity_blocks, 2);
        assert_eq!(h.rs_level, 2);
        assert_eq!(bytes.len(), 16 + (12 + 2) * BLOCK_BYTES);
    }
}
