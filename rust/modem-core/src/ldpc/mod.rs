//! LDPC WiMAX IEEE 802.16e, N=2304 (z=96).
//!
//! Rates: 1/2, 2/3, 3/4.
//! Encoder: systematic, exploiting dual-diagonal parity structure.
//! Decoder: Layered Normalized Min-Sum (LNMS).

pub mod wimax;
pub mod encoder;
pub mod decoder;

pub use crate::profile::LdpcRate;
pub use encoder::LdpcEncoder;
pub use decoder::LdpcDecoder;
