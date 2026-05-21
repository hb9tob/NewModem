pub use num_complex::Complex;

pub type Complex64 = Complex<f64>;

pub const AUDIO_RATE: u32 = 48_000;
pub const DATA_CENTER_HZ: f64 = 1100.0;

// TDM pilot structure
pub const D_SYMS: usize = 32;
pub const P_SYMS: usize = 2;

// Preamble
pub const N_PREAMBLE: usize = 256;
pub const PREAMBLE_SEED: u64 = 1234;

// RRC defaults
pub const RRC_SPAN_SYM: usize = 12;

// Peak normalization
pub const PEAK_NORMALIZE: f32 = 0.9;
