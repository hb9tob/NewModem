//! Energy dispersal (whitening) applied to the *information* bytes of every
//! transmitted block, so the modulator never emits a residual carrier from
//! low-entropy control/payload content.
//!
//! Wraps the tested G3RUH self-synchronising scrambler
//! ([`modem_core_base::scrambler`]). It is applied at the **byte level, before
//! FEC** at TX and **after FEC** at RX:
//!
//! - LDPC codewords (data + meta): [`crate::frame::encode_one_codeword`]
//!   scrambles the `k`-byte info block before LDPC encode; the RX decode paths
//!   ([`crate::rx_v2`], [`crate::v3_session`]) descramble the decoded info bytes.
//! - Marker control payload: [`crate::marker::encode_control_symbols`] scrambles
//!   the 12 ctrl bytes before Golay; [`crate::marker::decode_control_symbols`]
//!   descrambles them after Golay.
//!
//! Because whitening lives *outside* the FEC envelope, the LDPC/Golay decoders,
//! the interleaver, the soft demapper and the turbo FBF loop are untouched — they
//! all operate transparently on the whitened codeword. Only the recovered info
//! bytes are descrambled.
//!
//! The preamble, marker sync pattern, LMS warmup and pilots are **never**
//! scrambled: they are correlation / training references and are already
//! zero-mean.
//!
//! Enable/disable: whitening is ON by default. The initial state is seeded from
//! the `V3_NO_SCRAMBLER` env var (set = start disabled), and can be flipped at
//! runtime via [`set_enabled`] — e.g. a GUI "backward-compatibility" toggle, so
//! the operator can work an un-scrambled peer without relaunching. Both ends must
//! agree: this is a V4 wire change — a scrambled TX will not decode on an
//! un-scrambled RX and vice-versa.

use std::sync::atomic::{AtomicU8, Ordering};

use modem_core_base::scrambler as g3ruh;

// Tri-state so the env seed stays lazy while a runtime override still wins:
// 0 = not yet seeded, 1 = enabled, 2 = disabled.
static STATE: AtomicU8 = AtomicU8::new(0);

/// Whitening enabled? Seeds from `V3_NO_SCRAMBLER` on first read (default ON),
/// then honours any [`set_enabled`] override. Cheap enough for per-codeword loops.
fn enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = std::env::var_os("V3_NO_SCRAMBLER").is_none();
            // Idempotent if two threads race here — both compute the same seed.
            STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Flip whitening at runtime. Affects BOTH TX ([`scramble`]) and RX
/// ([`descramble`]) for all subsequent codewords, so it changes the on-air wire
/// format. `true` = whitening ON (default); `false` = identity, for interop with
/// an un-scrambled peer. Overrides the `V3_NO_SCRAMBLER` env seed.
pub fn set_enabled(on: bool) {
    STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
}

/// Current whitening state (seeds from the env on first call).
pub fn is_enabled() -> bool {
    enabled()
}

/// Whiten `bytes` before FEC (TX). Identity when disabled.
pub fn scramble(bytes: &[u8]) -> Vec<u8> {
    if enabled() {
        g3ruh::scramble(bytes)
    } else {
        bytes.to_vec()
    }
}

/// Recover the original bytes after FEC (RX). Identity when disabled.
pub fn descramble(bytes: &[u8]) -> Vec<u8> {
    if enabled() {
        g3ruh::descramble(bytes)
    } else {
        bytes.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_identity() {
        // Only meaningful when enabled (the default in the test process).
        let src: Vec<u8> = (0..64u16).map(|i| (i * 7 + 1) as u8).collect();
        assert_eq!(descramble(&scramble(&src)), src);
    }

    #[test]
    fn whitens_zeros() {
        // The dominant DC case: a low-entropy block must come out near-balanced.
        if !enabled() {
            return;
        }
        let s = scramble(&vec![0u8; 256]);
        let ones: usize = s.iter().map(|b| b.count_ones() as usize).sum();
        let ratio = ones as f64 / (s.len() * 8) as f64;
        assert!((ratio - 0.5).abs() < 0.05, "not whitened: {ratio}");
    }
}
