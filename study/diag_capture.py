#!/usr/bin/env python3
"""Quick OTA capture diagnostic.

Given a WAV produced by the GUI's raw-recording button, answers:
  - where is the actual signal vs noise?
  - does the signal clip (peak at unity) once isolated from squelch artefacts?
  - what does the spectrum look like in the signal region (tilt, bandwidth)?

Outputs:
  results/diag_<basename>_timeline.png   — peak & RMS per 100 ms window
  results/diag_<basename>_spectrum.png   — average PSD over the detected
                                            signal region

Run:
  /c/Users/tous/radioconda/python.exe study/diag_capture.py path/to/capture.wav
"""

import argparse
import os
import sys
import wave

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")


def load_wav(path):
    with wave.open(path, "r") as wf:
        nch = wf.getnchannels()
        sr = wf.getframerate()
        bits = wf.getsampwidth() * 8
        raw = wf.readframes(wf.getnframes())
    if bits == 16:
        s = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    elif bits == 32:
        s = np.frombuffer(raw, dtype=np.int32).astype(np.float64) / (2**31)
    else:
        raise ValueError(f"unsupported bit depth: {bits}")
    if nch > 1:
        s = s.reshape(-1, nch).mean(axis=1)
    return s, sr


def sliding_stats(samples, sr, win_sec=0.1, hop_sec=0.05):
    win = max(1, int(win_sec * sr))
    hop = max(1, int(hop_sec * sr))
    n = len(samples)
    peaks = []
    rmss = []
    times = []
    for i in range(0, n - win, hop):
        seg = samples[i : i + win]
        peaks.append(np.max(np.abs(seg)))
        rmss.append(np.sqrt(np.mean(seg**2)))
        times.append(i / sr)
    return np.array(times), np.array(peaks), np.array(rmss)


def detect_active_regions(times, peaks, silence_peak_thresh=0.05, clip_peak_thresh=0.95):
    """Split the capture into contiguous non-silence regions and classify
    each as 'clipping' (peak hits unity) or 'clean' (peak well below clip).
    Returns a list of dicts.
    """
    if len(times) < 5:
        return []
    active = peaks > silence_peak_thresh
    regions = []
    i = 0
    while i < len(active):
        if not active[i]:
            i += 1
            continue
        j = i
        while j < len(active) and active[j]:
            j += 1
        r_peaks = peaks[i:j]
        regions.append(
            {
                "t0": float(times[i]),
                "t1": float(times[j - 1]),
                "peak": float(np.max(r_peaks)),
                "mean_peak": float(np.mean(r_peaks)),
                "clipping": float(np.max(r_peaks)) >= clip_peak_thresh,
            }
        )
        i = j
    return regions


def pick_main_signal(regions):
    """Choose the longest 'clean' (non-clipping) region — that's almost
    certainly the modem TX. Falls back to the longest region if everything
    clips."""
    if not regions:
        return None
    clean = [r for r in regions if not r["clipping"]]
    candidates = clean if clean else regions
    return max(candidates, key=lambda r: r["t1"] - r["t0"])


def plot_timeline(times, peaks, rmss, regions, main, out_path):
    fig, ax = plt.subplots(figsize=(12, 4))
    ax.plot(times, 20 * np.log10(np.maximum(rmss, 1e-6)), label="RMS (dBFS)", color="#4fc3f7")
    ax.plot(
        times,
        20 * np.log10(np.maximum(peaks, 1e-6)),
        label="Peak (dBFS)",
        color="#ef5350",
        alpha=0.6,
    )
    ax.axhline(0, color="#888", linewidth=0.5, linestyle="--", label="0 dBFS (clip)")
    for r in regions:
        color = "#f44336" if r["clipping"] else "#4caf50"
        ax.axvspan(r["t0"], r["t1"], color=color, alpha=0.12)
    if main is not None:
        ax.axvspan(main["t0"], main["t1"], color="#4caf50", alpha=0.25, label="signal principal")
    ax.set_xlabel("t (s)")
    ax.set_ylabel("niveau (dBFS)")
    ax.set_ylim(-80, 5)
    ax.legend(loc="lower right", fontsize=8)
    ax.set_title(os.path.basename(out_path).replace("_timeline.png", ""))
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_path, dpi=110)
    plt.close(fig)


def plot_spectrum(samples, sr, out_path, title):
    # Welch PSD — average spectrum over the signal region
    from scipy.signal import welch

    f, psd = welch(samples, fs=sr, nperseg=min(8192, len(samples)), window="hann")
    psd_db = 10 * np.log10(np.maximum(psd, 1e-15))

    fig, ax = plt.subplots(figsize=(12, 4))
    ax.plot(f, psd_db, color="#4fc3f7")
    # Highlight the modem passband
    ax.axvspan(500, 1700, color="#ffd54f", alpha=0.12, label="bande modem (typ.)")
    ax.axvline(1100, color="#ff6060", linewidth=0.5, linestyle="--", label="fc = 1100 Hz")
    ax.axvline(2700, color="#888", linewidth=0.5, linestyle="--", label="NBFM audio coupure")
    ax.set_xlim(0, 4000)
    ax.set_xlabel("f (Hz)")
    ax.set_ylabel("PSD (dB)")
    ax.set_title(title)
    ax.legend(loc="upper right", fontsize=8)
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_path, dpi=110)
    plt.close(fig)


def verdict(samples, sr, regions, main):
    lines = []
    total_dur = len(samples) / sr
    lines.append(f"fichier : {total_dur:.1f} s @ {sr} Hz")
    lines.append(
        f"global  : peak={np.max(np.abs(samples)):.4f}, RMS={np.sqrt(np.mean(samples**2)):.4f}"
    )
    if not regions:
        lines.append("!! Aucune region active detectee (peak > 0.05). RX muet ?")
        return lines

    lines.append("")
    lines.append(f"{len(regions)} region(s) active(s) :")
    for k, r in enumerate(regions):
        tag = "CLIPPING" if r["clipping"] else "clean"
        marker = "  >>" if (main is not None and r is main) else "    "
        lines.append(
            f"{marker} [{k}] t={r['t0']:6.2f}-{r['t1']:6.2f}s  "
            f"dur={r['t1']-r['t0']:5.1f}s  peak={r['peak']:.3f}  "
            f"mean_peak={r['mean_peak']:.3f}  [{tag}]"
        )

    if main is None:
        lines.append("!! aucune region non-clippee trouvee")
        return lines

    i0, i1 = int(main["t0"] * sr), int(main["t1"] * sr)
    sig = samples[i0:i1]
    sig_peak = np.max(np.abs(sig))
    sig_rms = np.sqrt(np.mean(sig**2))
    sig_peak_db = 20 * np.log10(max(sig_peak, 1e-6))
    sig_rms_db = 20 * np.log10(max(sig_rms, 1e-6))
    crest = sig_peak / max(sig_rms, 1e-9)
    lines.append("")
    lines.append(
        f"signal principal : {main['t0']:.2f} -> {main['t1']:.2f} s "
        f"({main['t1']-main['t0']:.1f} s)"
    )
    lines.append(
        f"                 peak={sig_peak:.4f} ({sig_peak_db:+.1f} dBFS), "
        f"RMS={sig_rms:.4f} ({sig_rms_db:+.1f} dBFS), crest={crest:.2f}"
    )
    if sig_peak < 0.1:
        lines.append("                 !! niveau tres bas (<-20 dBFS) - remonter gain")
    elif sig_peak > 0.9:
        lines.append("                 !! proche du clipping - baisser gain")
    else:
        lines.append("                 niveau OK")

    # Spectrum centroid + tilt check
    from scipy.signal import welch

    f, psd = welch(sig, fs=sr, nperseg=min(8192, len(sig)), window="hann")
    band = (f > 500) & (f < 2200)
    if band.sum() > 0:
        centroid = np.sum(f[band] * psd[band]) / max(np.sum(psd[band]), 1e-12)
        lines.append(f"spectre : centroide bande modem ~ {centroid:.0f} Hz (attendu ~1100 Hz)")
        low = psd[(f > 700) & (f < 1100)].mean()
        high = psd[(f > 1100) & (f < 1500)].mean()
        tilt_db = 10 * np.log10(max(high, 1e-15) / max(low, 1e-15))
        verdict_tilt = "OK" if abs(tilt_db) < 3 else "canal colore"
        lines.append(
            f"        tilt 1100-1500 vs 700-1100 = {tilt_db:+.1f} dB [{verdict_tilt}]"
        )
    return lines


def main():
    p = argparse.ArgumentParser()
    p.add_argument("wav", help="chemin vers le WAV OTA capturé")
    p.add_argument("--outdir", default=RESULTS_DIR)
    args = p.parse_args()

    os.makedirs(args.outdir, exist_ok=True)

    samples, sr = load_wav(args.wav)
    base = os.path.splitext(os.path.basename(args.wav))[0]

    times, peaks, rmss = sliding_stats(samples, sr, 0.1, 0.05)
    regions = detect_active_regions(times, peaks)
    main = pick_main_signal(regions)

    timeline_path = os.path.join(args.outdir, f"diag_{base}_timeline.png")
    plot_timeline(times, peaks, rmss, regions, main, timeline_path)

    if main is not None:
        i0, i1 = int(main["t0"] * sr), int(main["t1"] * sr)
        spec_path = os.path.join(args.outdir, f"diag_{base}_spectrum.png")
        plot_spectrum(samples[i0:i1], sr, spec_path, f"{base} - spectre zone signal")

    print("\n".join(verdict(samples, sr, regions, main)))
    print(f"\nplots -> {args.outdir}/diag_{base}_*.png")


if __name__ == "__main__":
    main()
