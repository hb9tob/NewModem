#!/usr/bin/env python3
"""Combine the theoretical limits (modem_capacity.csv) with the MEASURED
byte-exact decode thresholds (turbo_sim awgnsweep, AWGN, ~33% RaptorQ repair,
1000 B payload) into one link-budget table for QO-100.

Implementation loss = measured threshold − constrained-capacity floor: the dB
this modem gives away vs the theoretical best for the same modulation + code.

Run:  /c/Users/tous/radioconda/python.exe study/modem_linkbudget.py
"""
import csv
import math
import os

HERE = os.path.dirname(__file__)
RES = os.path.join(HERE, "results")

# Measured Es/N0 decode thresholds (dB), >=80% byte-exact over the AWGN sweep
# (turbo_sim awgnsweep, carrier-tracking ON, ~33% repair, 1000 B). 1 dB grid.
# ULTRA shares QPSK 1/2 with ROBUST (same Es/N0 floor; only Rs/throughput differ).
MEASURED = {
    "ULTRA": 7.0, "ROBUST": 7.0, "NORMAL": 6.0,
    "HIGH": 12.0, "HIGH+": 16.0, "HIGH++": 18.0,
}


def main():
    cap = {}
    with open(os.path.join(RES, "modem_capacity.csv")) as f:
        for r in csv.DictReader(f):
            cap[r["mode"]] = r
    # ULTRA isn't in the capacity CSV (same constellation as ROBUST) — clone it
    # with ULTRA's own Rs/beta for throughput/BW.
    if "ULTRA" not in cap and "ROBUST" in cap:
        u = dict(cap["ROBUST"]); u["mode"] = "ULTRA"; u["Rs"] = "500"; cap["ULTRA"] = u

    order = ["ULTRA", "ROBUST", "NORMAL", "HIGH", "HIGH+", "HIGH++"]
    hdr = (f"{'mode':<8}{'mod':>10}{'R':>6}{'Rs':>6}{'thr_bps':>9}"
           f"{'EsN0_lim':>9}{'EsN0_meas':>10}{'impl_loss':>10}{'C/N_occ':>9}{'EbN0_meas':>10}")
    lines = [hdr, "-" * len(hdr)]
    rows = []
    for m in order:
        if m not in cap or m not in MEASURED:
            continue
        c = cap[m]
        M = int(c["M"]); R = float(c["R"]); beta = float(c["beta"]); Rs = float(c["Rs"])
        eta_sym = R * math.log2(M)
        thr = eta_sym * Rs
        lim = float(c["esn0_cm_db"])
        meas = MEASURED[m]
        loss = meas - lim
        cn_occ = meas - 10 * math.log10(1 + beta)            # link-budget C/N in (1+beta)Rs
        ebn0 = meas - 10 * math.log10(eta_sym)
        mod = {4: "QPSK", 8: "8PSK", 16: "16APSK", 32: "32APSK", 64: "64APSK"}[M]
        lines.append(f"{m:<8}{mod:>10}{R:>6.3f}{Rs:>6.0f}{thr:>9.0f}"
                     f"{lim:>9.2f}{meas:>10.1f}{loss:>10.1f}{cn_occ:>9.1f}{ebn0:>10.2f}")
        rows.append((m, mod, R, Rs, thr, lim, meas, loss, cn_occ, ebn0))
    out = "\n".join(lines)
    print(out)
    print("\nEsN0_lim = constrained-capacity floor (theoretical, this modulation+rate).")
    print("EsN0_meas = MEASURED byte-exact decode threshold (AWGN, ~33% repair, 1000 B).")
    print("impl_loss = EsN0_meas - EsN0_lim (dB the modem gives away).")
    print("C/N_occ = required C/N in the occupied band (1+beta)*Rs = the link-budget figure.")
    os.makedirs(RES, exist_ok=True)
    with open(os.path.join(RES, "modem_linkbudget.txt"), "w") as f:
        f.write(out + "\n")
    with open(os.path.join(RES, "modem_linkbudget.csv"), "w") as f:
        f.write("mode,mod,R,Rs,thr_bps,esn0_lim_db,esn0_meas_db,impl_loss_db,cn_occ_db,ebn0_meas_db\n")
        for r in rows:
            f.write(",".join(str(x) for x in r) + "\n")


if __name__ == "__main__":
    main()
