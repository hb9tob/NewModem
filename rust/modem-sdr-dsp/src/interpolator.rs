//! Polyphase FIR interpolator (×N).
//!
//! Equivalent to GNU Radio's `filter::interp_fir_filter_fff`. Reuses
//! the same taps as `decimator` (mirrored), since by Nyquist
//! reciprocity the same low-pass prototype works in both directions.
//!
//! TODO(impl): the ~80 LOC plus a round-trip test that ×11 then ÷11
//! recovers the original signal within numerical noise.
