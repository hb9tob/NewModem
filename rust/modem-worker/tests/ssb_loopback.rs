//! End-to-end proof that a modem signal transmitted as **USB-data SSB**
//! and received through the new [`SsbRxChain`] decodes with the *existing,
//! unmodified* modem RX — the architectural claim behind the SSB feature.
//!
//! No hardware: we synthesize the SDR IQ a real USB transmission would
//! produce. The modem TX (`V3Modem::encode_to_samples`) emits real audio
//! `a[n]` at 48 kHz with the xAPSK on its native 1100 Hz carrier — exactly
//! what an SSB rig's mic input sees. An ideal USB transmitter sends the
//! analytic signal `a + j·H{a}` (positive frequencies only); the SDR
//! captures it as complex IQ with the suppressed carrier at `f_sc` inside
//! the band. We build that IQ (FFT Hilbert → ZOH upsample ×12 → shift to
//! `f_sc`), run it through `SsbRxChain`, and decode the recovered audio.
//!
//! The assertion is byte-exact at the decoder output: decoding the SSB
//! front-end's audio yields the **same `RxV2Result.data`** as decoding the
//! original modem audio directly. LDPC recovers exact bytes or fails, so
//! equal `.data` means the SSB chain reproduced the waveform faithfully
//! enough for a bit-perfect decode — through both the Pluto (`lo_offset =
//! 0`) and SDRplay (`lo_offset = 75 kHz`) tuning paths.

use num_complex::Complex;
use rustfft::FftPlanner;

use modem_core::frame::effective_packet_count;
use modem_core::ldpc::encoder::LdpcEncoder;
use modem_core::profile::ProfileIndex;
use modem_core::rx_v2::rx_v2;
use modem_core::traits::{EncodeRequest, Modem};
use modem_core::v3_modem::V3Modem;
use modem_framing::payload_envelope::PayloadEnvelope;
use modem_framing::raptorq_codec::k_from_payload;
use modem_sdr_dsp::{SsbRxChain, SsbRxChainConfig};

const INPUT_RATE: u32 = 576_000;
const AUDIO_RATE: u32 = 48_000;
const DECIM: usize = (INPUT_RATE / AUDIO_RATE) as usize; // 12

fn fnv_short(data: &[u8]) -> u16 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h ^ (h >> 16)) as u16
}

/// Encode a deterministic payload to modem audio, mirroring the CLI/GUI
/// wire-payload assembly (see `cli_parity.rs`).
fn encode_modem_audio() -> Vec<f32> {
    let mut payload = vec![0u8; 200];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = ((i.wrapping_mul(31)).wrapping_add(7)) as u8;
    }
    let callsign = "HB9TEST";
    let filename = "payload.bin";
    let repair_pct: u32 = 5;
    let session_id: u32 = 0x51B_0001;

    let envelope = PayloadEnvelope::new(filename, callsign, payload).expect("envelope");
    let wire_payload = envelope.encode();

    let cfg = ProfileIndex::Normal.to_config();
    let k_bytes = LdpcEncoder::new(cfg.ldpc_rate).k() / 8;
    let k_src = k_from_payload(wire_payload.len(), k_bytes) as u32;
    let n_total = effective_packet_count(k_src + (k_src * repair_pct) / 100);

    let req = EncodeRequest {
        profile: "NORMAL",
        wire_payload: &wire_payload,
        session_id,
        mime_type: modem_framing::app_header::mime::BINARY,
        hash_short: fnv_short(&wire_payload),
        esi_start: 0,
        n_packets: n_total,
        vox_seconds: 0.0,
    };
    V3Modem.encode_to_samples(&req).expect("v3 encode")
}

/// Analytic signal `a + j·H{a}` via FFT (scipy.signal.hilbert convention:
/// positive frequencies doubled, so `Re{analytic} == a`).
fn analytic(a: &[f32]) -> Vec<Complex<f32>> {
    let n = a.len();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n);
    let ifft = planner.plan_fft_inverse(n);
    let mut buf: Vec<Complex<f32>> = a.iter().map(|&x| Complex::new(x, 0.0)).collect();
    fft.process(&mut buf);
    for (k, c) in buf.iter_mut().enumerate() {
        let h = if k == 0 {
            1.0
        } else if n % 2 == 0 && k == n / 2 {
            1.0
        } else if k < n.div_ceil(2) {
            2.0
        } else {
            0.0
        };
        *c *= h;
    }
    ifft.process(&mut buf);
    let scale = 1.0 / n as f32;
    buf.iter().map(|c| c * scale).collect()
}

/// Synthesize the SDR IQ for a USB transmission of `audio`, suppressed
/// carrier parked at `f_sc` Hz in the captured band. ZOH upsample ×DECIM
/// then shift by `f_sc` (the stage-1 channel LPF removes the ZOH images).
fn synth_usb_iq(audio: &[f32], f_sc: f32) -> Vec<Complex<f32>> {
    let an = analytic(audio);
    let mut iq = Vec::with_capacity(an.len() * DECIM);
    for &s in &an {
        for _ in 0..DECIM {
            iq.push(s);
        }
    }
    if f_sc != 0.0 {
        let two_pi = 2.0 * std::f32::consts::PI;
        for (k, c) in iq.iter_mut().enumerate() {
            let ph = two_pi * f_sc * k as f32 / INPUT_RATE as f32;
            *c *= Complex::new(ph.cos(), ph.sin());
        }
    }
    iq
}

#[test]
fn ssb_usb_roundtrip_decodes_byte_exact() {
    let audio = encode_modem_audio();
    let cfg = ProfileIndex::Normal.to_config();

    // Reference: the modem RX decoding its own clean audio.
    let reference = rx_v2(&audio, &cfg).expect("reference decode failed on clean modem audio");
    assert!(
        !reference.data.is_empty(),
        "reference decode recovered no data — test setup is wrong, not the SSB chain",
    );

    // Two tuning paths: Pluto (carrier at DC, lo_offset 0) and SDRplay
    // (carrier at −75 kHz, lo_offset +75 kHz → NCO brings it back to DC).
    for (label, f_sc, lo_offset) in [("pluto", 0.0_f32, 0.0_f32), ("sdrplay", -75_000.0, 75_000.0)] {
        let iq = synth_usb_iq(&audio, f_sc);
        let mut chain = SsbRxChain::new(SsbRxChainConfig::new(INPUT_RATE, 2700.0, lo_offset));
        let ssb_audio = chain.process(&iq);

        let got = rx_v2(&ssb_audio, &cfg)
            .unwrap_or_else(|| panic!("[{label}] SSB-demodulated audio failed to decode"));
        assert_eq!(
            got.data, reference.data,
            "[{label}] decoded payload differs from the clean-audio reference \
             (SSB front-end corrupted the waveform)",
        );
    }
}
