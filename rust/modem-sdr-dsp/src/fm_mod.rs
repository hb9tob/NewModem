//! FM modulator — Rust port of
//! `gr-analog/lib/frequency_modulator_fc_impl.cc`.
//!
//! Algorithm:
//!
//! ```text
//! phase[n] = phase[n-1] + sensitivity · x[n]      (mod 2π)
//! y[n]     = ( cos(phase[n]),  sin(phase[n]) )
//! sensitivity = 2·π · max_deviation / sample_rate
//! ```
//!
//! Phase is wrapped into `[-π, π]` periodically to keep
//! single-precision floats well-conditioned over long transmissions.
//!
//! TODO(impl): port the ~30 LOC and round-trip test against
//! `fm_demod::QuadratureDemod`.
