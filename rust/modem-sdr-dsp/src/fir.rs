//! Generic FIR filter — used for the audio-band low-pass that sits
//! between the FM modulator/demodulator and the modem (matching the
//! ~3 kHz upper bound of the NBFM appliance's audio filter).
//!
//! Equivalent to GNU Radio's `filter::fir_filter_fff`.
//!
//! TODO(impl): a thin transposed-form FIR (~40 LOC) plus the audio
//! LPF tap set.
