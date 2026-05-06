//! Polyphase FIR decimator (÷N).
//!
//! Equivalent to GNU Radio's `filter::fir_filter_ccc` configured as a
//! decimator. Taps are GR's `optfir.low_pass` output dumped offline
//! by `study/generate_taps.py` and embedded as `const` arrays in the
//! `taps` submodule, so the build does not depend on having GR
//! installed.
//!
//! Polyphase decomposition: with M = N (decimation factor), tap k of
//! phase φ is `h[φ + k·M]`. Output rate is input_rate / N. For NBFM
//! the lone supported ratio is N = 11 (528 → 48 kSa/s) with N = 20
//! (960 → 48 kSa/s) as the documented fallback if the AD9361 BBPLL
//! refuses to lock at 528 kSa/s.
//!
//! TODO(impl): the ~80 LOC + tests + tap arrays.
