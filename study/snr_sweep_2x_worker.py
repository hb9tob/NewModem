#!/usr/bin/env python3
"""SNR sweep for modem-2x, RX driven through `rx_worker2x` in cpal-sized
chunks (per the 2026-05-18 project rule).

Same channel as `snr_sweep_2x.py` (nbfm_channel_sim with drift / IF-AWGN /
optional OTA IR), but the RX side calls the `sweep_rx_via_worker`
example binary instead of `nbfm-modem rx`. That example pushes the WAV
through `rx_worker2x::spawn` in 2400-sample chunks and emits a
`RESULT_JSON {...}` line containing converged/total, sigma2, and
exact-match vs reference.

For each profile and each --if-noises level (cartesian-product with
--phase-walks), produces results.csv + a pretty stdout table.

Defaults are tuned so every 2x profile yields >= 2 PLHEADER cycles per
burst (the new SC bootstrap refuses to commit a single-cycle burst).

Run (after `cargo build --release -p modem-cli` AND
`cargo build --release -p modem-worker2x --example sweep_rx_via_worker`):

    python3 study/snr_sweep_2x_worker.py --drift-ppm 130
"""

import argparse
import json
import os
import struct
import subprocess
import sys
import time

import numpy as np

_here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _here)
from nbfm_channel_sim import simulate, AUDIO_RATE  # noqa: E402

PROFILES_DEFAULT = [
    "ULTRA2X", "ROBUST2X", "NORMAL2X", "HIGH2X",
    "HIGH+2X", "HIGH++2X", "HIGH56_2X", "HIGH+56_2X",
]
IF_NOISES_DEFAULT = [0.10, 0.20, 0.30, 0.40, 0.50, 0.65, 0.85]
PAYLOAD_BYTES_DEFAULT = 5_000
SEED_DEFAULT = 0xCAFE

ROOT = os.path.abspath(os.path.join(_here, ".."))
NBFM_MODEM_DEFAULT = os.path.join(ROOT, "rust", "target", "release", "nbfm-modem")
WORKER_RX_DEFAULT = os.path.join(
    ROOT, "rust", "target", "release", "examples", "sweep_rx_via_worker")


def read_wav_f32(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[0:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError(f"{path}: not a RIFF/WAVE file")
    pos = 12
    fmt_chunk = None
    data_chunk = None
    while pos + 8 <= len(data):
        chunk_id = data[pos:pos + 4]
        chunk_sz = struct.unpack("<I", data[pos + 4:pos + 8])[0]
        body = data[pos + 8:pos + 8 + chunk_sz]
        if chunk_id == b"fmt ":
            fmt_chunk = body
        elif chunk_id == b"data":
            data_chunk = body
            break
        pos += 8 + chunk_sz + (chunk_sz % 2)
    if fmt_chunk is None or data_chunk is None:
        raise ValueError(f"{path}: missing fmt/data chunk")
    fmt_tag, n_ch, _sr, _br, _blk, bits = struct.unpack(
        "<HHIIHH", fmt_chunk[:16])
    effective_tag = fmt_tag
    if fmt_tag == 0xFFFE and len(fmt_chunk) >= 26:
        effective_tag = struct.unpack("<H", fmt_chunk[24:26])[0]
    if effective_tag == 3 and bits == 32:
        samples = np.frombuffer(data_chunk, dtype=np.float32)
    elif effective_tag == 1 and bits == 16:
        samples = (np.frombuffer(data_chunk, dtype=np.int16)
                   .astype(np.float32) / 32768.0)
    else:
        raise ValueError(
            f"{path}: unsupported fmt_tag={fmt_tag} eff={effective_tag} "
            f"bits={bits}")
    if n_ch == 2:
        samples = samples[::2]
    return samples


def write_wav_f32(path, samples, sr=AUDIO_RATE):
    samples = np.asarray(samples, dtype=np.float32)
    n_samples = len(samples)
    byte_rate = sr * 4
    data_sz = n_samples * 4
    riff_sz = 36 + data_sz
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", riff_sz) + b"WAVE")
        f.write(b"fmt " + struct.pack("<IHHIIHH",
                                      16, 3, 1, sr, byte_rate, 4, 32))
        f.write(b"data" + struct.pack("<I", data_sz))
        f.write(samples.tobytes())


def run_tx(cli, profile, payload_path, wav_path, repair_pct):
    """CLI TX (uses tx_worker2x::encode_to_wav under the hood). Returns
    wall-clock seconds."""
    t0 = time.time()
    subprocess.run(
        [cli, "tx",
         "-i", payload_path, "-o", wav_path,
         "--family", "2x", "-p", profile,
         "--callsign", "HB9TOB", "--vox", "0.5",
         "--repair-pct", str(repair_pct)],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return time.time() - t0


def run_rx_worker(worker_bin, profile, wav_path, ref_path):
    """Worker RX (sweep_rx_via_worker — 2400-sample cpal-style chunks).
    Returns the parsed RESULT_JSON dict."""
    r = subprocess.run(
        [worker_bin, profile, wav_path, ref_path],
        capture_output=True, text=True)
    for line in r.stdout.splitlines():
        if line.startswith("RESULT_JSON "):
            return json.loads(line[len("RESULT_JSON "):])
    return {
        "profile": profile, "wav": wav_path,
        "error": "no RESULT_JSON in worker stdout",
        "converged": None, "total": None, "sigma2": None,
        "decoded_bytes": None, "file_complete_seen": False, "exact": None,
    }


def main():
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nbfm-modem", default=NBFM_MODEM_DEFAULT)
    ap.add_argument("--worker-rx", default=WORKER_RX_DEFAULT)
    ap.add_argument("--out-dir", default="results/snr_sweep_2x_worker")
    ap.add_argument("--profiles", nargs="+", default=PROFILES_DEFAULT)
    ap.add_argument("--if-noises", nargs="+", type=float, default=IF_NOISES_DEFAULT)
    ap.add_argument("--phase-walks", nargs="+", type=float, default=[0.0])
    ap.add_argument("--payload-bytes", type=int, default=PAYLOAD_BYTES_DEFAULT)
    ap.add_argument("--seed", type=int, default=SEED_DEFAULT)
    ap.add_argument("--keep-wavs", action="store_true")
    ap.add_argument("--drift-ppm", type=float, default=0.0)
    ap.add_argument("--thermal-ppm", type=float, default=0.0)
    ap.add_argument("--thermal-period", type=float, default=120.0)
    ap.add_argument("--drift-ramp-ppm-per-s", type=float, default=0.0,
                    help="Linear drift ramp (ppm/s) added on top of "
                         "static --drift-ppm. Models a SoC clock heating "
                         "up linearly during a long burst.")
    ap.add_argument("--tx-clip", type=float, default=None)
    ap.add_argument("--repair-pct", type=int, default=30,
                    help="RaptorQ repair pct on top of K (default 30; pass "
                         "0 to expose raw CW miss rate — but ensure the "
                         "payload yields >= 2 PLHEADER cycles per burst).")
    args = ap.parse_args()

    if not os.path.exists(args.nbfm_modem):
        print(f"ERROR: nbfm-modem binary not found at {args.nbfm_modem}",
              file=sys.stderr)
        return 1
    if not os.path.exists(args.worker_rx):
        print(f"ERROR: worker rx binary not found at {args.worker_rx}.\n"
              f"  Build with: cd rust && cargo build --release "
              f"-p modem-worker2x --example sweep_rx_via_worker",
              file=sys.stderr)
        return 1

    os.makedirs(args.out_dir, exist_ok=True)
    rng = np.random.RandomState(args.seed)
    payload = rng.bytes(args.payload_bytes)
    payload_path = os.path.join(args.out_dir, "payload.bin")
    with open(payload_path, "wb") as f:
        f.write(payload)
    print(f"Payload: {args.payload_bytes} bytes, seed=0x{args.seed:X} "
          f"→ {payload_path}")
    print(f"Profiles: {args.profiles}")
    drift_desc = f"drift={args.drift_ppm:+.1f} ppm"
    if args.drift_ramp_ppm_per_s != 0.0:
        drift_desc += f" + ramp {args.drift_ramp_ppm_per_s:+.3f} ppm/s"
    if args.thermal_ppm > 0:
        drift_desc += (f" + thermal ±{args.thermal_ppm:.1f} ppm "
                       f"/ {args.thermal_period:.0f} s")
    print(f"IF noises: {args.if_noises}  ({drift_desc})")

    csv_path = os.path.join(args.out_dir, "results.csv")
    with open(csv_path, "w") as csv_f:
        # `total` is now the honest expected DATA CW count (from the
        # AppHeader's RaptorQ K + default repair, rounded up by the
        # tail-fill). `total_seen` is the legacy "CWs the RX actually
        # attempted" count — useful for cycle-loss diagnostics. When
        # AppHeader never lands, `total` falls back to `total_seen` and
        # `total_is_expected = 0` so the row is flagged.
        csv_f.write(
            "profile,if_noise,phase_walk,injected_drift_ppm,"
            "injected_thermal_ppm,sigma2,snr_est_db,sigma2_data_scatter,"
            "es_data_scatter,data_scatter_n,converged,total,total_seen,"
            "total_is_expected,"
            "decoded_bytes,exact,file_complete_seen,error,total_time_s,"
            "estimated_drift_ppm,cycles\n")

        header = (f"{'profile':<10} {'if_n':>5} {'phw':>5} {'σ²':>9} "
                  f"{'SNRdB':>6} {'σ²scatter':>10} {'conv':>9} "
                  f"{'rx_B':>6} {'exact':>5} {'t(s)':>6}")
        print("\n" + header)
        print("-" * len(header))

        for profile in args.profiles:
            safe = profile.replace("+", "p")
            tx_wav = os.path.join(args.out_dir, f"tx_{safe}.wav")
            try:
                tx_time = run_tx(args.nbfm_modem, profile, payload_path,
                                 tx_wav, args.repair_pct)
            except subprocess.CalledProcessError as e:
                print(f">>> TX {profile}: FAILED ({e})")
                continue
            tx_audio = read_wav_f32(tx_wav)
            print(f"\n>>> TX {profile}: {len(tx_audio)} samples "
                  f"({len(tx_audio)/AUDIO_RATE:.2f} s, build {tx_time:.1f}s)")

            for if_noise in args.if_noises:
                for phase_walk in args.phase_walks:
                    t0 = time.time()
                    sim_kwargs = dict(
                        if_noise_voltage=if_noise,
                        drift_ppm=args.drift_ppm,
                        thermal_ppm=args.thermal_ppm,
                        thermal_period_s=args.thermal_period,
                        drift_ramp_ppm_per_s=args.drift_ramp_ppm_per_s,
                        phase_walk_rad_per_sqrt_s=phase_walk,
                        start_delay_s=0.5,
                        rng_seed=args.seed + int(if_noise * 1000)
                                  + int(phase_walk * 10000),
                        verbose=False,
                    )
                    if args.tx_clip is not None:
                        sim_kwargs["tx_hard_clip"] = args.tx_clip
                    ch_audio = simulate(tx_audio, **sim_kwargs)
                    tag = f"if{if_noise:.2f}_pw{phase_walk:.3f}"
                    ch_wav = os.path.join(args.out_dir, f"ch_{safe}_{tag}.wav")
                    write_wav_f32(ch_wav, ch_audio.astype(np.float32))

                    info = run_rx_worker(args.worker_rx, profile, ch_wav,
                                         payload_path)
                    t_total = time.time() - t0

                    sigma2 = info.get("sigma2")
                    sigma2_scatter = info.get("sigma2_data_scatter")
                    es_scatter = info.get("es_data_scatter")
                    scatter_n = info.get("data_scatter_n")
                    if sigma2 is not None and sigma2 > 0:
                        snr_est = -10.0 * np.log10(sigma2)
                    else:
                        snr_est = float("nan")
                    converged = info.get("converged")
                    total = info.get("total")
                    total_seen = info.get("total_seen")
                    total_is_expected = info.get("total_is_expected", False)
                    decoded_bytes = info.get("decoded_bytes")
                    exact = info.get("exact")
                    fc_seen = info.get("file_complete_seen", False)
                    err = info.get("error")

                    def csv_cell(v):
                        if v is None:
                            return ""
                        if isinstance(v, bool):
                            return "1" if v else "0"
                        return str(v)

                    est_drift = info.get("final_drift_ppm")
                    cycles = info.get("cycles")
                    csv_f.write(",".join([
                        profile, str(if_noise), str(phase_walk),
                        str(args.drift_ppm), str(args.thermal_ppm),
                        csv_cell(sigma2), str(snr_est),
                        csv_cell(sigma2_scatter), csv_cell(es_scatter),
                        csv_cell(scatter_n),
                        csv_cell(converged), csv_cell(total),
                        csv_cell(total_seen),
                        csv_cell(total_is_expected),
                        csv_cell(decoded_bytes), csv_cell(exact),
                        csv_cell(fc_seen),
                        (err or "").replace(",", ";"),
                        f"{t_total:.2f}",
                        csv_cell(est_drift), csv_cell(cycles),
                    ]) + "\n")
                    csv_f.flush()

                    def fmt(v, w, p=None):
                        if v is None or (isinstance(v, float)
                                         and np.isnan(v)):
                            return f"{'-':>{w}}"
                        if p is not None:
                            return f"{v:>{w}.{p}f}"
                        return f"{v:>{w}}"

                    # Tag the ratio with `*` when total is the
                    # fallback (data_cws_total — no AppHeader), so an
                    # operator scanning stdout sees at a glance that the
                    # denominator is the symbol-gated count, not the
                    # honest expected.
                    if converged is not None and total is not None:
                        flag = "" if total_is_expected else "*"
                        conv_str = f"{converged}/{total}{flag}"
                    else:
                        conv_str = "-/-"
                    print(
                        f"{profile:<10} {if_noise:>5.2f} {phase_walk:>5.2f} "
                        f"{fmt(sigma2, 9, 5)} {fmt(snr_est, 6, 1)} "
                        f"{fmt(sigma2_scatter, 10, 5)} "
                        f"{conv_str:>9} "
                        f"{str(decoded_bytes or '-'):>6} "
                        f"{'Y' if exact else 'N':>5} "
                        f"{t_total:>5.1f}"
                    )

                    if not args.keep_wavs and os.path.exists(ch_wav):
                        os.remove(ch_wav)

            if not args.keep_wavs and os.path.exists(tx_wav):
                os.remove(tx_wav)

    print(f"\nResults CSV: {csv_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
