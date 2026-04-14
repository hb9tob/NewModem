//! Assemblage et desassemblage d'une frame complete (avec support multi-frame).
//!
//! Structure globale d'une transmission :
//!   [Frame 0] [Frame 1] ... [Frame N-1 avec FLAG_LAST]
//!
//! Chaque frame = [Header 16 B | Data blocs | Parity blocs RS]
//!   Frame 0  body : filename_length | filename | payload_chunk_0 | body_crc32
//!   Frame i  body : payload_chunk_i | body_crc32
//!
//! Chaque frame a son propre CRC32 (detection independante des autres).

use crate::crc::crc32;
use crate::header::{FrameHeader, FLAG_HAS_FILENAME, FLAG_LAST};
use crate::rs::{encode as rs_encode, parity_blocks_for, RsError};
use crate::ModemMode;

/// Taille d'un bloc de data = 1 codeword LDPC info.
pub const BLOCK_BYTES: usize = 90;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("filename trop long (max 255 octets UTF-8), recu {0}")]
    FilenameTooLong(usize),
    #[error("body corrompu : CRC32 mismatch")]
    BadBodyCrc,
    #[error("frame trop courte")]
    Truncated,
    #[error("payload trop gros pour un seul fichier (plus de 65535 frames)")]
    TooManyFrames,
    #[error("RS: {0}")]
    Rs(#[from] RsError),
}

/// Construit le body d'une frame a partir du chunk de payload et, si present,
/// du filename (frame 0 seulement).
pub fn build_body_chunk(filename: Option<&str>, payload_chunk: &[u8])
    -> Result<Vec<u8>, FrameError>
{
    let mut body = Vec::new();
    if let Some(fn_) = filename {
        let fn_bytes = fn_.as_bytes();
        if fn_bytes.len() > 255 {
            return Err(FrameError::FilenameTooLong(fn_bytes.len()));
        }
        body.push(fn_bytes.len() as u8);
        body.extend_from_slice(fn_bytes);
    }
    body.extend_from_slice(payload_chunk);
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    Ok(body)
}

/// Extrait (filename_opt, payload_chunk) d'un body decode.
pub fn parse_body_chunk(body: &[u8], has_filename: bool)
    -> Result<(Option<String>, Vec<u8>), FrameError>
{
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
    let (filename, payload) = if has_filename {
        let fn_len = body[0] as usize;
        if body.len() < 1 + fn_len + 4 {
            return Err(FrameError::Truncated);
        }
        let fn_ = String::from_utf8_lossy(&body[1..1 + fn_len]).to_string();
        (Some(fn_), body[1 + fn_len..crc_pos].to_vec())
    } else {
        (None, body[..crc_pos].to_vec())
    };
    Ok((filename, payload))
}

/// Taille max K (blocs data) pour un rs_level donne, contraint par GF(2^8).
pub fn max_k_for_rs_level(rs_level: u8) -> usize {
    // K + M <= 256 avec M = ceil(K/r), r = 16,8,4 pour level 1,2,3
    match rs_level {
        0 => 256,  // pas de contrainte RS, mais on cap pour rester raisonnable
        1 => 240,  // K + K/16 <= 256  ->  K <= 240
        2 => 227,  // K + K/8 <= 256   ->  K <= 227
        3 => 204,  // K + K/4 <= 256   ->  K <= 204
        _ => 0,
    }
}

/// Nombre max d'octets "body" (avant RS) qu'on peut mettre dans une frame.
pub fn max_body_bytes_for_rs_level(rs_level: u8) -> usize {
    max_k_for_rs_level(rs_level) * BLOCK_BYTES
}

/// Taille utile max du payload par frame (apres overhead filename et CRC32).
/// filename_len = 0 pour les frames de continuation.
pub fn max_payload_per_frame(rs_level: u8, filename_len: usize) -> usize {
    let body_max = max_body_bytes_for_rs_level(rs_level);
    // Overhead : fn_len byte (si present) + filename + crc32
    let overhead = if filename_len > 0 { 1 + filename_len + 4 } else { 4 };
    body_max.saturating_sub(overhead)
}

/// Divise un body en blocs de BLOCK_BYTES (padding zero du dernier).
fn split_into_blocks(body: &[u8]) -> Vec<Vec<u8>> {
    let n = (body.len() + BLOCK_BYTES - 1) / BLOCK_BYTES;
    let mut blocks = Vec::with_capacity(n);
    for i in 0..n {
        let start = i * BLOCK_BYTES;
        let end = (start + BLOCK_BYTES).min(body.len());
        let mut blk = vec![0u8; BLOCK_BYTES];
        blk[..end - start].copy_from_slice(&body[start..end]);
        blocks.push(blk);
    }
    blocks
}

/// Assemble une frame individuelle (header + body chunks + RS parity).
///
/// Usage avance : prefere `build_frames()` qui fait le split multi-frame auto.
pub fn build_frame(
    mode: ModemMode,
    rs_level: u8,
    frame_id: u16,
    total_frames: u16,
    is_last: bool,
    filename: Option<&str>,
    payload_chunk: &[u8],
) -> Result<Vec<u8>, FrameError> {
    let body = build_body_chunk(filename, payload_chunk)?;
    let data_blocks = split_into_blocks(&body);
    let n_data = data_blocks.len();
    let n_parity = parity_blocks_for(n_data, rs_level);
    let all_blocks = rs_encode(&data_blocks, rs_level)?;
    assert_eq!(all_blocks.len(), n_data + n_parity);

    let mut flags = 0u8;
    if is_last { flags |= FLAG_LAST; }
    if filename.is_some() { flags |= FLAG_HAS_FILENAME; }

    let header = FrameHeader {
        version: crate::header::VERSION,
        mode_code: mode.to_code(),
        rs_level,
        n_data_blocks: n_data as u16,
        n_parity_blocks: n_parity as u16,
        frame_id,
        total_frames,
        flags,
    };

    let header_bytes = header.encode();
    let mut out = Vec::with_capacity(header_bytes.len() + all_blocks.len() * BLOCK_BYTES);
    out.extend_from_slice(&header_bytes);
    for blk in &all_blocks {
        out.extend_from_slice(blk);
    }
    Ok(out)
}

/// Split un payload en N frames optimalement selon le mode + rs_level.
/// Frame 0 porte le filename. Retourne Vec<Vec<u8>> prets pour le modulateur.
pub fn build_frames(
    mode: ModemMode,
    rs_level: u8,
    filename: &str,
    payload: &[u8],
) -> Result<Vec<Vec<u8>>, FrameError> {
    let fn_bytes = filename.as_bytes();
    if fn_bytes.len() > 255 {
        return Err(FrameError::FilenameTooLong(fn_bytes.len()));
    }

    // Capacite frame 0 (avec filename) et continuation (sans filename)
    let cap_frame_0 = max_payload_per_frame(rs_level, fn_bytes.len());
    let cap_cont = max_payload_per_frame(rs_level, 0);

    // Calcule le nombre de frames necessaires
    let payload_len = payload.len();
    let n_frames = if payload_len <= cap_frame_0 {
        1
    } else {
        1 + (payload_len - cap_frame_0 + cap_cont - 1) / cap_cont
    };
    if n_frames > 65535 {
        return Err(FrameError::TooManyFrames);
    }
    let total = n_frames as u16;

    let mut frames = Vec::with_capacity(n_frames);
    let mut cursor = 0usize;
    for i in 0..n_frames {
        let (chunk_cap, fn_opt) = if i == 0 {
            (cap_frame_0, Some(filename))
        } else {
            (cap_cont, None)
        };
        let remaining = payload_len - cursor;
        let chunk_size = chunk_cap.min(remaining);
        let chunk = &payload[cursor..cursor + chunk_size];
        cursor += chunk_size;
        let is_last = i == n_frames - 1;
        let frame = build_frame(
            mode, rs_level, i as u16, total, is_last, fn_opt, chunk)?;
        frames.push(frame);
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::HEADER_SIZE;

    #[test]
    fn body_roundtrip_with_filename() {
        let body = build_body_chunk(Some("img.png"), b"hello").unwrap();
        let (fn_, pl) = parse_body_chunk(&body, true).unwrap();
        assert_eq!(fn_, Some("img.png".to_string()));
        assert_eq!(pl, b"hello");
    }

    #[test]
    fn body_roundtrip_continuation() {
        let body = build_body_chunk(None, b"continuation data").unwrap();
        let (fn_, pl) = parse_body_chunk(&body, false).unwrap();
        assert_eq!(fn_, None);
        assert_eq!(pl, b"continuation data");
    }

    #[test]
    fn single_frame_small_payload() {
        let frames = build_frames(
            ModemMode::Qam16R34_1500, 2, "test.bin", &[0xAA; 100]).unwrap();
        assert_eq!(frames.len(), 1);
        let h = FrameHeader::decode(&frames[0][..HEADER_SIZE]).unwrap();
        assert_eq!(h.frame_id, 0);
        assert_eq!(h.total_frames, 1);
        assert!(h.is_last_frame());
        assert!(h.has_filename());
    }

    #[test]
    fn multi_frame_image() {
        // 30 ko payload + level 2 -> doit donner 2 frames
        let frames = build_frames(
            ModemMode::Qam16R34_1500, 2, "image.avif", &vec![0xBB; 30000]).unwrap();
        assert_eq!(frames.len(), 2);

        let h0 = FrameHeader::decode(&frames[0][..HEADER_SIZE]).unwrap();
        let h1 = FrameHeader::decode(&frames[1][..HEADER_SIZE]).unwrap();
        assert_eq!(h0.frame_id, 0);
        assert_eq!(h1.frame_id, 1);
        assert_eq!(h0.total_frames, 2);
        assert_eq!(h1.total_frames, 2);
        assert!(h0.has_filename());
        assert!(!h1.has_filename());
        assert!(!h0.is_last_frame());
        assert!(h1.is_last_frame());
    }

    #[test]
    fn cap_level_bounds() {
        // Level 3 : 204 blocs max
        assert_eq!(max_k_for_rs_level(3), 204);
        assert_eq!(max_body_bytes_for_rs_level(3), 204 * 90);
    }
}
