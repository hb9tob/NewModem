#!/usr/bin/env python3
"""Theoretical SNR limits per modem mode for the QO-100 link budget.

For each mode (constellation dumped from the Rust modem + its LDPC code rate)
this computes the RIGOROUS coded-modulation limit:

  - Constrained AWGN capacity C(Es/N0) of the actual M-point constellation
    (uniform input, Monte-Carlo mutual information).
  - The minimum Es/N0 where C(Es/N0) == R * log2(M)  — the theoretical floor
    for THIS modulation + code rate (DVB-S2 thresholds sit ~0.7-1 dB above it).
  - The unconstrained Shannon limit for the same spectral efficiency, for ref.

No invented numbers: constellation points come from the Rust constellation
builder (study/constel/*.txt via `turbo_sim consteldump`), the rest is computed.

Reports per mode: M, R, eta (bits/symb & bits/s/Hz), Es/N0, Eb/N0, and C/N in
the occupied bandwidth (1+beta)*Rs — the figure to drop into a link budget.

Run:  /c/Users/tous/radioconda/python.exe study/modem_capacity.py
"""
import glob
import math
import os
import numpy as np

CONSTEL_DIR = os.path.join(os.path.dirname(__file__), "constel")
RESULTS_DIR = os.path.join(os.path.dirname(__file__), "results")
L = 20000          # Monte-Carlo noise samples per constellation point
RNG = np.random.default_rng(12345)


def load_mode(path):
    with open(path) as f:
        head = f.readline().strip().lstrip("#").split()
    kv = dict(tok.split("=") for tok in head)
    pts = np.loadtxt(path, comments="#")
    x = pts[:, 0] + 1j * pts[:, 1]
    x = x / np.sqrt(np.mean(np.abs(x) ** 2))   # normalise to unit Es
    return {
        "name": kv["profile"],
        "M": int(kv["M"]),
        "R": int(kv["rate_num"]) / int(kv["rate_den"]),
        "Rs": float(kv["symbol_rate"]),
        "beta": float(kv["beta"]),
        "x": x,
    }


def cm_capacity(x, esn0_lin):
    """Monte-Carlo constrained AWGN capacity (bits/symbol), uniform input.

    y = x + n, n ~ CN(0, sigma2), Es=1 -> sigma2 = 1/esn0_lin.
    I(X;Y) = log2(M) - E_{i,n}[ log2 sum_j exp(-(|x_i+n-x_j|^2 - |n|^2)/sigma2) ]

    Looped over the M sent points (each step is (L, M)), so memory stays O(L*M)
    instead of O(M^2*L) — 64-APSK at L=40k would otherwise need ~2.6 GB.
    """
    m = len(x)
    sigma2 = 1.0 / esn0_lin
    acc = 0.0
    for i in range(m):
        n = (RNG.standard_normal(L) + 1j * RNG.standard_normal(L)) * math.sqrt(sigma2 / 2)
        y = x[i] + n                         # (L,)
        d2 = np.abs(y[:, None] - x[None, :]) ** 2   # (L, M): |y - x_j|^2
        expo = -(d2 - (np.abs(n) ** 2)[:, None]) / sigma2
        mx = expo.max(axis=1, keepdims=True)
        lse = mx[:, 0] + np.log(np.exp(expo - mx).sum(axis=1))
        acc += (lse / math.log(2.0)).mean()
    return math.log2(m) - acc / m


def esn0_for_target(x, target_bits):
    """Bisect Es/N0 (dB) where the constrained capacity == target_bits."""
    lo, hi = -10.0, 35.0
    for _ in range(26):
        mid = 0.5 * (lo + hi)
        c = cm_capacity(x, 10 ** (mid / 10))
        if c < target_bits:
            lo = mid
        else:
            hi = mid
    return 0.5 * (lo + hi)


def main():
    files = sorted(glob.glob(os.path.join(CONSTEL_DIR, "*.txt")))
    rows = []
    for path in files:
        mode = load_mode(path)
        M, R, beta, Rs = mode["M"], mode["R"], mode["beta"], mode["Rs"]
        eta_sym = R * math.log2(M)                       # bits/symbol (coded modulation)
        esn0_cm_db = esn0_for_target(mode["x"], eta_sym)  # constrained limit
        esn0_cm = 10 ** (esn0_cm_db / 10)
        ebn0_cm_db = esn0_cm_db - 10 * math.log10(eta_sym)
        # Unconstrained Shannon for the same bits/symbol: Es/N0 = 2^eta - 1.
        esn0_sh_db = 10 * math.log10(2 ** eta_sym - 1)
        ebn0_sh_db = esn0_sh_db - 10 * math.log10(eta_sym)
        # C/N in the occupied bandwidth (1+beta)*Rs: C/N = (Es/N0)/(1+beta).
        cn_occ_db = esn0_cm_db - 10 * math.log10(1 + beta)
        eta_hz = eta_sym / (1 + beta)                    # bits/s/Hz
        thr = eta_sym * Rs                               # gross coded bits/s
        rows.append(dict(
            name=mode["name"], M=M, R=R, beta=beta, Rs=Rs,
            eta_sym=eta_sym, eta_hz=eta_hz, thr=thr,
            esn0_cm=esn0_cm_db, ebn0_cm=ebn0_cm_db,
            esn0_sh=esn0_sh_db, ebn0_sh=ebn0_sh_db, cn_occ=cn_occ_db,
        ))

    rows.sort(key=lambda r: r["esn0_cm"])
    hdr = (f"{'mode':<8}{'M':>4}{'R':>6}{'Rs':>7}{'eta_b/s':>9}{'eta_bHz':>9}"
           f"{'thr_bps':>9}{'EsN0_lim':>10}{'EbN0_lim':>10}{'EsN0_Sh':>9}{'C/N_occ':>9}")
    print(hdr)
    print("-" * len(hdr))
    lines = [hdr, "-" * len(hdr)]
    for r in rows:
        line = (f"{r['name']:<8}{r['M']:>4}{r['R']:>6.3f}{r['Rs']:>7.0f}"
                f"{r['eta_sym']:>9.2f}{r['eta_hz']:>9.2f}{r['thr']:>9.0f}"
                f"{r['esn0_cm']:>10.2f}{r['ebn0_cm']:>10.2f}{r['esn0_sh']:>9.2f}{r['cn_occ']:>9.2f}")
        print(line)
        lines.append(line)
    print("\nEsN0_lim = constrained-capacity floor (dB) of THIS modulation at code rate R.")
    print("EbN0_lim = same per info-bit. EsN0_Sh = unconstrained Shannon. C/N_occ = in (1+beta)*Rs.")
    print("Real modem decode threshold (measured by awgnsweep) should sit ~0.7-1.5 dB above EsN0_lim.")

    os.makedirs(RESULTS_DIR, exist_ok=True)
    with open(os.path.join(RESULTS_DIR, "modem_capacity.txt"), "w") as f:
        f.write("\n".join(lines) + "\n")
    # CSV for the synthesis step.
    with open(os.path.join(RESULTS_DIR, "modem_capacity.csv"), "w") as f:
        f.write("mode,M,R,Rs,beta,eta_sym,eta_hz,thr_bps,esn0_cm_db,ebn0_cm_db,esn0_shannon_db,cn_occ_db\n")
        for r in rows:
            f.write(f"{r['name']},{r['M']},{r['R']:.4f},{r['Rs']:.0f},{r['beta']},"
                    f"{r['eta_sym']:.4f},{r['eta_hz']:.4f},{r['thr']:.1f},"
                    f"{r['esn0_cm']:.3f},{r['ebn0_cm']:.3f},{r['esn0_sh']:.3f},{r['cn_occ']:.3f}\n")


if __name__ == "__main__":
    main()
