//! Quadrature FM demodulator — Rust port of
//! `gr-analog/lib/quadrature_demod_cf_impl.cc`.
//!
//! Algorithm (one line of math, plus state):
//!
//! ```text
//! y[n] = gain * atan2( I[n]·Q[n-1] − Q[n]·I[n-1],
//!                      I[n]·I[n-1] + Q[n]·Q[n-1] )
//! gain = sample_rate / (2·π · max_deviation)
//! ```
//!
//! The `atan2` argument is the imaginary / real part of
//! `z[n] · conj(z[n-1])`, i.e. the phase difference between
//! consecutive complex samples. For a constant-envelope FM signal
//! this difference is proportional to the instantaneous frequency
//! offset; scaling by `gain` recovers the modulating audio sample.
//!
//! TODO(impl): port the ~30 LOC and add the round-trip test against
//! `fm_mod::FrequencyMod`.
