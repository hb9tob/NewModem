//! Narrow contracts that SDR backends implement.
//!
//! These mirror the seams already exposed by `modem-io` for cpal
//! (`SampleSink` for TX, an mpsc `Receiver<Vec<f32>>` for RX). Keeping
//! the trait definitions here lets `modem-pluto` / `modem-rtlsdr` /
//! `modem-sdrplay` plug into `modem-worker` interchangeably with the
//! existing soundcard backend without `modem-worker` depending on any
//! single SDR crate.
//!
//! TODO(scaffold): fill in once the first backend (`modem-pluto`) is
//! drafted — the trait shape is easier to get right with one
//! concrete implementation already in hand.
