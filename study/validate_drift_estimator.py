#!/usr/bin/env python3
"""
Drift-estimator validation harness for the 2x modem (slice 2x22).

Pipeline:
  1. Generate random payload sized to produce ~60s of HIGH+2X TX audio
  2. Encode → WAV via `nbfm-modem tx`
  3. Apply `nbfm_channel_sim` with:
       - drift_ppm = 130 (static offset)
       - drift_ramp_ppm_per_s = 0.1 (linear evolution)
       - if_noise = 0.20 (≈ 20 dB SNR post-demod)
     Dumps the per-100ms injected drift trajectory to JSON.
  4. Decode with `RX2X_LOG_DRIFT_TICK=1` and parse the per-chunk
     `[rx2x-drift-tick]` lines.
  5. Plot injected vs estimated drift on the same time axis.

Outputs `results/<tag>/`:
  - tx.wav, channel.wav, decoded.bin (intermediate artefacts)
  - injected_drift.json (sim trace, 0.1s tick)
  - estimated_drift.csv (modem on-line LS, per-chunk)
  - drift_validation.png (the plot)

Usage:
  python3 study/validate_drift_estimator.py
  python3 study/validate_drift_estimator.py --duration-s 30 --static-ppm 60
"""

import argparse
import csv
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

_here = Path(__file__).parent
sys.path.insert(0, str(_here))
from nbfm_channel_sim import simulate, AUDIO_RATE  # noqa: E402
import scipy.io.wavfile as wavfile

# Defaults — overridable from the CLI
PROFILE_DEFAULT = "HIGH+2X"
DURATION_S_DEFAULT = 60.0
STATIC_PPM_DEFAULT = 130.0
RAMP_PPM_PER_S_DEFAULT = 0.1
IF_NOISE_DEFAULT = 0.20  # ≈ 20 dB SNR post-demod per snr_sweep_2x cheat sheet

# Empirical: HIGH+2X carries ~5.3 kbits/s of payload (V3 HighPlus parity).
# That's ~660 B/s → 60s ≈ 40 kB. Use 60 kB for margin so the TX WAV
# overshoots 60s rather than undershooting (we crop on the channel side).
PAYLOAD_BPS_GUESS = {
    "ULTRA2X": 600, "ROBUST2X": 900, "NORMAL2X": 1700, "HIGH2X": 2700,
    "HIGH+2X": 5300, "HIGH++2X": 7000, "HIGH56_2X": 3000,
    "HIGH+56_2X": 6000,
}

REPO_ROOT = _here.parent
CLI = REPO_ROOT / "rust" / "target" / "release" / "nbfm-modem"

_DRIFT_TICK_RE = re.compile(
    r"\[rx2x-drift-tick\] t=([\d.eE+\-]+) drift_ppm=([\d.eE+\-]+|NaN) sofs=(\d+)")


def load_wav_f32(path: Path) -> np.ndarray:
    """scipy handles both PCM16 and the IEEE-float WAVE_FORMAT_EXTENSIBLE
    variant the Rust TX emits — the stdlib `wave` module rejects the
    latter with `unknown extended format`."""
    sr, data = wavfile.read(str(path))
    assert sr == AUDIO_RATE, f"expect {AUDIO_RATE}, got {sr}"
    if data.dtype == np.int16:
        return data.astype(np.float32) / 32768.0
    if data.dtype == np.int32:
        return data.astype(np.float32) / (1 << 31)
    if data.dtype == np.float32 or data.dtype == np.float64:
        return data.astype(np.float32)
    raise TypeError(f"unsupported WAV dtype {data.dtype}")


def wav_len_s(path: Path) -> float:
    sr, data = wavfile.read(str(path))
    return data.shape[0] / sr


def write_wav_f32(path: Path, samples: np.ndarray, sr: int = AUDIO_RATE):
    pcm = np.clip(samples, -1.0, 1.0)
    pcm = (pcm * 32767).astype(np.int16)
    wavfile.write(str(path), sr, pcm)


def run_tx(profile: str, payload_path: Path, wav_path: Path) -> float:
    """Returns the produced WAV duration (s)."""
    subprocess.run(
        [str(CLI), "tx",
         "-i", str(payload_path), "-o", str(wav_path),
         "--family", "2x", "-p", profile,
         "--callsign", "HB9TOB", "--vox", "0",
         "--repair-pct", "30"],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return wav_len_s(wav_path)


def run_rx_with_drift_tick(profile: str, wav_path: Path,
                           out_path: Path) -> tuple[list[tuple[float, float, int]], dict]:
    """Decode with RX2X_LOG_DRIFT_TICK=1 and parse per-chunk drift
    estimates. Returns (ticks, summary) where ticks = list of
    (t_s, drift_ppm_or_nan, sofs) and summary is the final stats line."""
    env = os.environ.copy()
    env["RX2X_LOG_DRIFT_TICK"] = "1"
    r = subprocess.run(
        [str(CLI), "rx",
         "-i", str(wav_path), "-o", str(out_path),
         "--family", "2x", "-p", profile],
        capture_output=True, text=True, env=env)

    ticks: list[tuple[float, float, int]] = []
    summary = {"exit_code": r.returncode, "decoded_bytes": None,
               "converged": None, "total": None, "sigma2": None,
               "snr_data_db": None}
    for line in r.stderr.splitlines():
        m = _DRIFT_TICK_RE.search(line)
        if m:
            t = float(m.group(1))
            ppm = float("nan") if m.group(2) == "NaN" else float(m.group(2))
            sofs = int(m.group(3))
            ticks.append((t, ppm, sofs))
            continue
        m_dec = re.search(
            r"Decoded: (\d+) bytes, (\d+)/(\d+) CWs converged.*?σ²=([\d.eE+\-]+)",
            line)
        if m_dec:
            summary["decoded_bytes"] = int(m_dec.group(1))
            summary["converged"] = int(m_dec.group(2))
            summary["total"] = int(m_dec.group(3))
            summary["sigma2"] = float(m_dec.group(4))
            continue
        m_snr = re.search(r"Data-scatter SNR: ([\d.eE+\-]+) dB", line)
        if m_snr:
            summary["snr_data_db"] = float(m_snr.group(1))
    if r.returncode != 0:
        # Surface stderr tail when CLI fails, but still return ticks
        # if we parsed any (the drift sweep can fail mid-burst).
        print("--- RX stderr tail ---", file=sys.stderr)
        for line in r.stderr.splitlines()[-15:]:
            print(line, file=sys.stderr)
    return ticks, summary


def plot_validation(injected: dict, ticks: list, summary: dict,
                    static_ppm: float, ramp_ppm_per_s: float,
                    if_noise: float, profile: str, out_path: Path):
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(11, 7),
                                    gridspec_kw={"height_ratios": [3, 1]},
                                    sharex=True)

    # Top: injected vs estimated drift
    t_inj = np.array(injected["t_s"])
    ppm_inj = np.array(injected["ppm_injected"])
    ax1.plot(t_inj, ppm_inj, "C0-", lw=1.6, label="Injected (sim)")

    if ticks:
        t_est = np.array([t for t, _, _ in ticks])
        ppm_est = np.array([p for _, p, _ in ticks])
        sofs_est = np.array([s for _, _, s in ticks])
        # The LS estimator returns 0.0 until ≥ 3 SOFs are validated —
        # mask those as "not-yet-usable" so they don't disgrace the plot.
        ready_mask = sofs_est >= 3
        ax1.plot(t_est[ready_mask], ppm_est[ready_mask],
                 "C1.-", ms=5, lw=1.2, label="Estimated (LS, ≥3 SOFs)")
        not_ready = ~ready_mask
        if not_ready.any():
            ax1.plot(t_est[not_ready], ppm_est[not_ready],
                     "C3x", ms=6, alpha=0.6,
                     label="Estimate not yet usable (<3 SOFs)")

    ax1.set_ylabel("Drift (ppm)")
    ax1.set_title(
        f"Drift estimator validation — {profile}, "
        f"static {static_ppm:+.0f} ppm + ramp {ramp_ppm_per_s:+.2f} ppm/s, "
        f"if_noise={if_noise}\n"
        f"Decoded {summary.get('decoded_bytes')} bytes, "
        f"{summary.get('converged')}/{summary.get('total')} CWs converged, "
        f"σ²={summary.get('sigma2')}, "
        f"SNR_data={summary.get('snr_data_db')} dB")
    ax1.grid(alpha=0.3)
    ax1.legend(loc="upper left")

    # Bottom: estimation error
    if ticks:
        # Resample injected onto estimator ticks for the error trace.
        inj_at_est = np.interp(t_est, t_inj, ppm_inj)
        err = ppm_est - inj_at_est
        ax2.plot(t_est[ready_mask], err[ready_mask],
                 "C2-", lw=1.0, label="Error (est − inj)")
        ax2.axhline(0, color="k", lw=0.7, alpha=0.5)
        ax2.set_ylabel("Error (ppm)")
        ax2.grid(alpha=0.3)
        ax2.legend(loc="upper left")
        rms = float(np.sqrt(np.mean(err[ready_mask] ** 2))) \
            if ready_mask.any() else float("nan")
        ax2.set_title(f"RMS error (post-acquisition): {rms:.2f} ppm")

    ax2.set_xlabel("Time (s)")
    plt.tight_layout()
    plt.savefig(out_path, dpi=130)
    plt.close(fig)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default=PROFILE_DEFAULT)
    ap.add_argument("--duration-s", type=float, default=DURATION_S_DEFAULT)
    ap.add_argument("--static-ppm", type=float, default=STATIC_PPM_DEFAULT)
    ap.add_argument("--ramp-ppm-per-s", type=float,
                    default=RAMP_PPM_PER_S_DEFAULT)
    ap.add_argument("--if-noise", type=float, default=IF_NOISE_DEFAULT)
    ap.add_argument("--seed", type=int, default=0xC0FFEE)
    ap.add_argument("--out", type=str, default=None,
                    help="Output directory (default: results/drift_validate_<tag>)")
    args = ap.parse_args()

    bps = PAYLOAD_BPS_GUESS.get(args.profile, 5000)
    payload_bytes = int(bps * args.duration_s / 8 * 1.15)  # 15 % over-shoot

    tag = (f"drift_validate_{args.profile.replace('+', 'p').replace('/', '_')}_"
           f"s{int(args.static_ppm)}_r{args.ramp_ppm_per_s:.2f}_"
           f"d{int(args.duration_s)}s_n{args.if_noise:.2f}")
    out_dir = Path(args.out) if args.out else REPO_ROOT / "results" / tag
    out_dir.mkdir(parents=True, exist_ok=True)

    payload_path = out_dir / "payload.bin"
    tx_wav = out_dir / "tx.wav"
    ch_wav = out_dir / "channel.wav"
    decoded = out_dir / "decoded.bin"
    drift_json = out_dir / "injected_drift.json"
    drift_csv = out_dir / "estimated_drift.csv"
    plot_png = out_dir / "drift_validation.png"

    rng = np.random.default_rng(args.seed)
    payload_path.write_bytes(rng.bytes(payload_bytes))
    print(f"[1/5] payload: {payload_bytes} bytes, seed=0x{args.seed:X}")

    print(f"[2/5] TX → {tx_wav.name} ...")
    t0 = time.time()
    tx_dur = run_tx(args.profile, payload_path, tx_wav)
    print(f"      tx duration = {tx_dur:.2f}s "
          f"(wall {time.time() - t0:.1f}s)")

    # If TX overshoots target duration, crop to keep the ramp coherent
    # with the asked-for window.
    if tx_dur > args.duration_s + 2.0:
        audio = load_wav_f32(tx_wav)
        n_keep = int(args.duration_s * AUDIO_RATE)
        audio = audio[:n_keep]
        write_wav_f32(tx_wav, audio)
        print(f"      cropped to {args.duration_s:.0f}s")

    print(f"[3/5] channel → {ch_wav.name} "
          f"(static {args.static_ppm:+.1f} + ramp {args.ramp_ppm_per_s:+.2f} ppm/s, "
          f"if_noise={args.if_noise})")
    audio_in = load_wav_f32(tx_wav)
    audio_out = simulate(
        audio_in,
        if_noise_voltage=args.if_noise,
        drift_ppm=args.static_ppm,
        drift_ramp_ppm_per_s=args.ramp_ppm_per_s,
        drift_trace_path=str(drift_json),
        rng_seed=args.seed,
        verbose=True,
    )
    write_wav_f32(ch_wav, audio_out)

    print(f"[4/5] RX with drift-tick logging ...")
    t0 = time.time()
    ticks, summary = run_rx_with_drift_tick(args.profile, ch_wav, decoded)
    print(f"      rx wall = {time.time() - t0:.1f}s, "
          f"{len(ticks)} drift ticks captured")
    print(f"      summary: {summary}")

    with open(drift_csv, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t_s", "drift_ppm", "sofs"])
        for t, ppm, sofs in ticks:
            w.writerow([f"{t:.3f}", f"{ppm:+.5f}", sofs])

    print(f"[5/5] plotting → {plot_png.name}")
    with open(drift_json) as f:
        injected = json.load(f)
    plot_validation(injected, ticks, summary,
                    args.static_ppm, args.ramp_ppm_per_s,
                    args.if_noise, args.profile, plot_png)
    print(f"\nResults in: {out_dir}")
    print(f"  Plot: {plot_png}")


if __name__ == "__main__":
    main()
