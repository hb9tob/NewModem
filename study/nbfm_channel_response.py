#!/usr/bin/env python3
"""
Measure the frequency response and noise floor of an NBFM channel
using the GNU Radio blocks (analog.nbfm_tx / nbfm_rx).

Method:
  1. Generate a sinusoidal sweep (tone by tone) from 100 Hz to 4 kHz.
  2. Pass each tone through the NBFM TX -> channel -> NBFM RX chain.
  3. Measure output amplitude vs input -> frequency response.
  4. Measure the noise floor (silent input).
  5. Plot the results.

Typical radio-amateur NBFM parameters:
  - Max deviation: +/- 2.5 kHz (standard NBFM)
  - Pre-emphasis / de-emphasis: 750 us (EU) or 300 us
  - Useful audio band: ~300 Hz - 3 kHz
"""

import numpy as np
import matplotlib
matplotlib.use("Agg")  # no display needed for saving
import matplotlib.pyplot as plt
from gnuradio import gr, blocks, analog, filter as gr_filter


# ---------------------------------------------------------------------------
# Parameters
# ---------------------------------------------------------------------------
AUDIO_RATE = 48000          # audio sample rate (Hz)
IF_RATE = 480000            # IF sample rate (10x audio)
FM_DEVIATION = 5000.0       # max FM deviation (Hz) - GNU Radio nbfm_tx default
TAU = 75e-6                 # pre-emphasis constant (s) - GNU Radio default (75 us)
TONE_DURATION = 0.5         # duration of each tone (s)
SETTLE_SKIP = 0.1           # seconds to skip at the start (transient)
TONE_AMPLITUDE = 0.3        # input tone amplitude (0..1)

# Frequencies to sweep
FREQS = np.concatenate([
    np.arange(100, 500, 50),
    np.arange(500, 3500, 100),
    np.arange(3500, 4100, 100),
])

# Channel SNR (dB) - white Gaussian noise added on the IF
AUDIO_SNR_DB = 20.0          # target SNR at the audio output (not IF)


def make_nbfm_channel(audio_in, audio_snr_db=AUDIO_SNR_DB):
    """
    Build a flowgraph:
      audio_in -> NBFM TX -> NBFM RX -> + audio noise -> audio_out

    Noise is added AFTER the FM demodulator, directly on the audio, to
    simulate the SNR as measured at the transceiver output. Faithfully
    models the receiver's thermal + phase noise as it appears at the
    speaker / line-out connector.

    The noise level is calibrated to obtain the target SNR on a
    TONE_AMPLITUDE tone (reference signal power).
    """
    tb = gr.top_block("nbfm_channel")

    # Source: input audio vector (float)
    src = blocks.vector_source_f(audio_in.tolist(), False)

    # --- TX ---
    nbfm_tx = analog.nbfm_tx(
        audio_rate=AUDIO_RATE,
        quad_rate=IF_RATE,
        tau=TAU,
        max_dev=FM_DEVIATION,
        fh=-1.0,  # no high-pass filter on the audio
    )

    # --- RX ---
    nbfm_rx = analog.nbfm_rx(
        audio_rate=AUDIO_RATE,
        quad_rate=IF_RATE,
        tau=TAU,
        max_dev=FM_DEVIATION,
    )

    # --- Post-demodulation audio noise ---
    # SNR = 20*log10(signal_rms / noise_rms)
    # For a tone of amplitude A, RMS = A/sqrt(2)
    # noise_rms = signal_rms / 10^(snr/20)
    # noise_source_f generates with std = amplitude argument
    signal_rms = TONE_AMPLITUDE / np.sqrt(2.0)
    noise_voltage = signal_rms / (10.0 ** (audio_snr_db / 20.0))
    audio_noise = analog.noise_source_f(analog.GR_GAUSSIAN, noise_voltage, 0)
    audio_add = blocks.add_ff()

    # Sink: collect the output samples
    sink = blocks.vector_sink_f()

    # Connections
    tb.connect(src, nbfm_tx)
    tb.connect(nbfm_tx, nbfm_rx)
    tb.connect(nbfm_rx, (audio_add, 0))
    tb.connect(audio_noise, (audio_add, 1))
    tb.connect(audio_add, sink)

    return tb, sink


def measure_tone_response(freq, amplitude=TONE_AMPLITUDE, snr_db=AUDIO_SNR_DB):
    """Send a tone at `freq` Hz and measure the output RMS amplitude."""
    n_samples = int(TONE_DURATION * AUDIO_RATE)
    t = np.arange(n_samples) / AUDIO_RATE
    audio_in = (amplitude * np.sin(2 * np.pi * freq * t)).astype(np.float32)

    tb, sink = make_nbfm_channel(audio_in, snr_db)
    tb.run()

    audio_out = np.array(sink.data())

    # Skip the initial transient
    skip = int(SETTLE_SKIP * AUDIO_RATE)
    audio_out = audio_out[skip:]

    if len(audio_out) == 0:
        return 0.0, 0.0

    # Fundamental RMS via correlation (more robust than global RMS)
    t_out = np.arange(len(audio_out)) / AUDIO_RATE
    ref_sin = np.sin(2 * np.pi * freq * t_out)
    ref_cos = np.cos(2 * np.pi * freq * t_out)
    amp_out = 2.0 * np.sqrt(
        (np.mean(audio_out * ref_sin)) ** 2
        + (np.mean(audio_out * ref_cos)) ** 2
    )

    # Residual noise RMS
    reconstructed = amp_out * np.sin(
        2 * np.pi * freq * t_out + np.arctan2(
            np.mean(audio_out * ref_cos),
            np.mean(audio_out * ref_sin),
        )
    )
    noise_rms = np.sqrt(np.mean((audio_out - reconstructed) ** 2))

    return amp_out, noise_rms


def measure_noise_floor(snr_db=AUDIO_SNR_DB):
    """Measure the output noise without an input signal."""
    n_samples = int(TONE_DURATION * AUDIO_RATE)
    audio_in = np.zeros(n_samples, dtype=np.float32)

    tb, sink = make_nbfm_channel(audio_in, snr_db)
    tb.run()

    audio_out = np.array(sink.data())
    skip = int(SETTLE_SKIP * AUDIO_RATE)
    audio_out = audio_out[skip:]

    if len(audio_out) == 0:
        return 0.0

    return np.sqrt(np.mean(audio_out ** 2))


def main():
    print(f"=== NBFM channel characterization ===")
    print(f"Audio rate: {AUDIO_RATE} Hz, IF rate: {IF_RATE} Hz")
    print(f"Deviation: +/-{FM_DEVIATION} Hz, Tau: {TAU*1e6:.0f} us")
    print(f"Channel SNR: {AUDIO_SNR_DB} dB")
    print(f"Sweep: {len(FREQS)} frequencies from {FREQS[0]} to {FREQS[-1]} Hz")
    print()

    # --- Noise-floor measurement ---
    print("Measuring noise floor (silence)...")
    noise_floor = measure_noise_floor()
    print(f"  Output RMS noise (silence): {noise_floor:.6f}")
    print()

    # --- Frequency sweep ---
    amplitudes = []
    noises = []
    for i, f in enumerate(FREQS):
        amp, noise = measure_tone_response(f)
        amplitudes.append(amp)
        noises.append(noise)
        gain_db = 20 * np.log10(amp / TONE_AMPLITUDE) if amp > 0 else -100
        print(f"  [{i+1:3d}/{len(FREQS)}] {f:5.0f} Hz : "
              f"gain = {gain_db:+6.1f} dB, noise = {noise:.6f}")

    amplitudes = np.array(amplitudes)
    noises = np.array(noises)

    # Relative gain in dB (normalized by input amplitude)
    gains_db = 20 * np.log10(amplitudes / TONE_AMPLITUDE + 1e-12)
    # Normalize to 0 dB at max gain
    gains_db -= np.max(gains_db)

    snr_per_tone = 20 * np.log10(amplitudes / (noises + 1e-12))

    # --- Save data ---
    data = np.column_stack([FREQS, amplitudes, gains_db, noises, snr_per_tone])
    header = "freq_hz  amplitude  gain_db  noise_rms  snr_db"
    np.savetxt("../results/nbfm_channel_response.csv", data,
               header=header, fmt="%.6f", delimiter=",")
    print(f"\nData saved to results/nbfm_channel_response.csv")

    # --- Plots ---
    fig, axes = plt.subplots(3, 1, figsize=(10, 12), sharex=True)

    # 1. Frequency response
    ax = axes[0]
    ax.plot(FREQS, gains_db, "b.-", linewidth=1.5, markersize=4)
    ax.set_ylabel("Relative gain (dB)")
    ax.set_title(
        f"NBFM channel frequency response\n"
        f"(dev. +/-{FM_DEVIATION:.0f} Hz, tau={TAU*1e6:.0f} us, "
        f"SNR={AUDIO_SNR_DB:.0f} dB)"
    )
    ax.grid(True, alpha=0.3)
    ax.axhline(-3, color="r", linestyle="--", alpha=0.5, label="-3 dB")
    ax.legend()

    # 2. Per-tone SNR
    ax = axes[1]
    ax.plot(FREQS, snr_per_tone, "g.-", linewidth=1.5, markersize=4)
    ax.set_ylabel("SNR (dB)")
    ax.set_title("SNR per frequency")
    ax.grid(True, alpha=0.3)

    # 3. Absolute output amplitude vs input
    ax = axes[2]
    ax.plot(FREQS, amplitudes, "m.-", linewidth=1.5, markersize=4,
            label="Output")
    ax.axhline(TONE_AMPLITUDE, color="k", linestyle="--", alpha=0.5,
               label=f"Input ({TONE_AMPLITUDE})")
    ax.axhline(noise_floor, color="r", linestyle=":", alpha=0.5,
               label=f"Noise floor ({noise_floor:.4f})")
    ax.set_xlabel("Frequency (Hz)")
    ax.set_ylabel("RMS amplitude")
    ax.set_title("Absolute amplitudes")
    ax.legend()
    ax.grid(True, alpha=0.3)

    plt.tight_layout()
    plt.savefig("../results/nbfm_channel_response.png", dpi=150)
    print("Plot saved to results/nbfm_channel_response.png")
    plt.close()

    # --- Summary ---
    bw_mask = gains_db >= -3.0
    if np.any(bw_mask):
        bw_low = FREQS[bw_mask][0]
        bw_high = FREQS[bw_mask][-1]
        print(f"\n-3 dB bandwidth: {bw_low:.0f} - {bw_high:.0f} Hz")
    print(f"Max gain: {np.max(gains_db):.1f} dB at {FREQS[np.argmax(gains_db)]:.0f} Hz")
    print(f"Mean SNR (300-3000 Hz): "
          f"{np.mean(snr_per_tone[(FREQS>=300) & (FREQS<=3000)]):.1f} dB")


if __name__ == "__main__":
    main()
