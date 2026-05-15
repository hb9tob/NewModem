#!/usr/bin/env python3
"""SNR sweep for the modem-2x family.

For each `--profile` and each `--if-noise` level, runs the full end-to-end
chain: nbfm-modem CLI TX (V4 wire format) → `nbfm_channel_sim.simulate()`
(NBFM TX + IF AWGN + NBFM RX) → nbfm-modem CLI RX → byte-level comparison.

Produces results.csv + a pretty-printed table on stdout. No GR Companion
needed; we drive the GR top_block from `simulate()` via Python.

Channel-sim defaults are unchanged from nbfm_channel_sim's calibration
(clock drift static -16 ppm OFF here, tx_hard_clip 0.55, post-LPF 2000 Hz,
sub-audio HPF 300 Hz). What varies is `if_noise_voltage`, the std-dev of
the complex Gaussian noise injected at IF.

Calibrated cheat sheet (from modem_ldpc_snr_sweep.py and SNR estimates
from RxResult2x.sigma2_data):

    if_noise  →  Es/N0 (dB)   →  comment
    -------     -----------      ----------------------
    0.20         ~20-22         clean, every profile decodes
    0.35         ~15-17         HIGH+ marginal, HIGH++ borderline
    0.55         ~10-12         HIGH OK, HIGH+ marginal, HIGH++ fails

Run (after `cargo build -p modem-cli --release` and `pip install
matplotlib` on top of radioconda's GR):

    python study/snr_sweep_2x.py
    python study/snr_sweep_2x.py --profiles HIGH2X HIGH+2X --if-noises 0.30 0.50
    python study/snr_sweep_2x.py --payload-bytes 1000  # smoke run
"""

import argparse
import os
import re
import struct
import subprocess
import sys
import time

import numpy as np

# Make nbfm_channel_sim importable from the same study/ directory.
_here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _here)
from nbfm_channel_sim import simulate, AUDIO_RATE, load_ir_from_signature  # noqa: E402

# --- Defaults ---------------------------------------------------------------

PROFILES_DEFAULT = ["HIGH2X", "HIGH+2X", "HIGH++2X"]
IF_NOISES_DEFAULT = [0.20, 0.35, 0.55]  # ~20 / ~16 / ~11 dB Es/N0
PAYLOAD_BYTES_DEFAULT = 10_000
SEED_DEFAULT = 0xCAFE
NBFM_MODEM_DEFAULT = os.path.join(
    _here, "..", "rust", "target", "release", "nbfm-modem")

# --- WAV I/O ----------------------------------------------------------------

def read_wav_f32(path: str) -> np.ndarray:
    """Read a mono WAV. Accepts 32-bit IEEE float (what nbfm-modem TX writes)
    or 16-bit PCM (what nbfm_channel_sim.write_wav writes). Returns a 1-D
    float32 numpy array in [-1, 1]."""
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
    fmt_tag, n_ch, _sr, _byte_rate, _blk, bits = struct.unpack(
        "<HHIIHH", fmt_chunk[:16])
    # hound's WavWriter with SampleFormat::Float emits WAVE_FORMAT_EXTENSIBLE
    # (fmt_tag=0xFFFE). The actual format is the first 2 bytes of the
    # SubFormat GUID at offset 24 (cbSize=22, validBitsPerSample, ChannelMask,
    # SubFormat[16]).
    effective_tag = fmt_tag
    if fmt_tag == 0xFFFE and len(fmt_chunk) >= 26:
        # SubFormat starts at offset 8 (cbSize)+2 + 2 (validBits) + 4 (chanMask) = 16
        # Actually layout: bytes 16-17=cbSize, 18-19=validBits, 20-23=chanMask, 24-39=SubFormat GUID.
        sub_tag = struct.unpack("<H", fmt_chunk[24:26])[0]
        effective_tag = sub_tag
    if effective_tag == 3 and bits == 32:
        samples = np.frombuffer(data_chunk, dtype=np.float32)
    elif effective_tag == 1 and bits == 16:
        samples = (np.frombuffer(data_chunk, dtype=np.int16)
                   .astype(np.float32) / 32768.0)
    elif effective_tag == 1 and bits == 32:
        samples = (np.frombuffer(data_chunk, dtype=np.int32)
                   .astype(np.float32) / 2147483648.0)
    else:
        raise ValueError(f"{path}: unsupported fmt_tag={fmt_tag} "
                         f"effective={effective_tag} bits={bits}")
    if n_ch == 2:
        samples = samples[::2]
    return samples


def write_wav_f32(path: str, samples: np.ndarray, sr: int = AUDIO_RATE) -> None:
    """Write a mono 32-bit IEEE float WAV — same format nbfm-modem TX
    writes, so nbfm-modem RX reads it back without conversion."""
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


# --- CLI wrappers -----------------------------------------------------------

def run_tx(cli: str, profile: str, payload_path: str, wav_path: str,
           family: str = "2x") -> float:
    """Generate WAV TX for the given profile. Returns wall-clock seconds."""
    t0 = time.time()
    subprocess.run(
        [cli, "tx",
         "-i", payload_path, "-o", wav_path,
         "--family", family, "-p", profile,
         "--callsign", "HB9TOB", "--vox", "0"],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return time.time() - t0


_RX_LINE_RE_V4 = re.compile(
    r"Decoded: (\d+) bytes, (\d+)/(\d+) CWs converged, "
    r"(\d+) PLHEADER cycles, σ²=([\d.eE+\-]+), EOT_seen=(\w+)")
_RX_LINE_RE_V3 = re.compile(
    r"Decoded: (\d+) bytes, (\d+)/(\d+) LDPC blocks converged, "
    r"(\d+) segments, (\d+) lost, sigma²=([\d.eE+\-]+)")
_RX_LINE_RE_SPLIT = re.compile(
    r"Channel σ² split: radial=([\d.eE+\-]+), "
    r"tangential=([\d.eE+\-]+), ratio_R/T=([\d.eE+\-]+)")


def run_rx(cli: str, profile: str, wav_path: str, out_path: str,
           family: str = "2x") -> dict:
    """RX one WAV. Parses CLI stderr. Supports both V3 ('legacy', LDPC
    blocks + segments + sigma²) and V4 ('2x', CWs + PLHEADER cycles +
    σ²) output formats."""
    r = subprocess.run(
        [cli, "rx",
         "-i", wav_path, "-o", out_path,
         "--family", family, "-p", profile],
        capture_output=True, text=True)
    info = {
        "exit_code": r.returncode,
        "decoded_bytes": None, "converged": None, "total": None,
        "cycles": None, "sigma2": None, "eot": None,
        "sigma2_radial": None, "sigma2_tangential": None, "ratio_rt": None,
    }
    # Both lines (Decoded + Channel σ² split) appear on V4 RX. V3 RX only
    # emits the Decoded line. Walk every line and absorb whatever matches.
    for line in r.stderr.splitlines():
        m4 = _RX_LINE_RE_V4.search(line)
        if m4 and info["decoded_bytes"] is None:
            info["decoded_bytes"] = int(m4.group(1))
            info["converged"] = int(m4.group(2))
            info["total"] = int(m4.group(3))
            info["cycles"] = int(m4.group(4))
            info["sigma2"] = float(m4.group(5))
            info["eot"] = (m4.group(6) == "true")
            continue
        m3 = _RX_LINE_RE_V3.search(line)
        if m3 and info["decoded_bytes"] is None:
            info["decoded_bytes"] = int(m3.group(1))
            info["converged"] = int(m3.group(2))
            info["total"] = int(m3.group(3))
            info["cycles"] = int(m3.group(4))  # = "segments" for V3
            info["sigma2"] = float(m3.group(6))
            info["eot"] = None  # V3 has no EOT
            continue
        ms = _RX_LINE_RE_SPLIT.search(line)
        if ms:
            info["sigma2_radial"] = float(ms.group(1))
            info["sigma2_tangential"] = float(ms.group(2))
            info["ratio_rt"] = float(ms.group(3))
            continue
    return info


def measure_ber(payload_bytes: bytes, decoded_path: str) -> tuple[float, bool]:
    """Return (byte-level BER on first min(len(ref), len(rx)) bytes,
    byte-exact full match)."""
    if not os.path.exists(decoded_path):
        return 1.0, False
    with open(decoded_path, "rb") as f:
        rx = f.read()
    if not rx:
        return 1.0, False
    n = min(len(payload_bytes), len(rx))
    if n == 0:
        return 1.0, False
    ref_b = np.frombuffer(payload_bytes[:n], dtype=np.uint8)
    rx_b = np.frombuffer(rx[:n], dtype=np.uint8)
    bit_diff = np.unpackbits(np.bitwise_xor(ref_b, rx_b))
    ber = float(bit_diff.sum()) / (n * 8)
    exact = (rx == payload_bytes)
    return ber, exact


# --- Main -------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nbfm-modem", default=NBFM_MODEM_DEFAULT,
                    help=f"Path to nbfm-modem binary (default {NBFM_MODEM_DEFAULT})")
    ap.add_argument("--out-dir", default="results/snr_sweep_2x",
                    help="Output directory for payload, intermediate WAVs, "
                         "and results.csv")
    ap.add_argument("--profiles", nargs="+", default=PROFILES_DEFAULT,
                    help=f"Profiles to test (default {' '.join(PROFILES_DEFAULT)})")
    ap.add_argument("--family", default="2x", choices=["2x", "legacy"],
                    help="Wire-format family for the CLI (default 2x). Use "
                         "'legacy' to benchmark V3 against the same channel "
                         "(profile names use V3 naming: HIGH++, HIGH+, etc., "
                         "without the 2X suffix).")
    ap.add_argument("--if-noises", nargs="+", type=float, default=IF_NOISES_DEFAULT,
                    help=f"IF noise voltages (default {IF_NOISES_DEFAULT})")
    ap.add_argument("--payload-bytes", type=int, default=PAYLOAD_BYTES_DEFAULT,
                    help=f"Payload size (default {PAYLOAD_BYTES_DEFAULT})")
    ap.add_argument("--seed", type=int, default=SEED_DEFAULT,
                    help=f"PRNG seed for payload + channel start delay "
                         f"(default 0x{SEED_DEFAULT:X})")
    ap.add_argument("--keep-wavs", action="store_true",
                    help="Don't delete intermediate WAVs (debugging)")
    ap.add_argument("--drift-ppm", type=float, default=0.0,
                    help="Clock drift applied by the channel sim (default 0 "
                         "to isolate the SNR variable; pass -16 to reproduce "
                         "the calibrated sound-card drift)")
    ap.add_argument("--thermal-ppm", type=float, default=0.0,
                    help="Amplitude (peak) of thermal drift oscillation in "
                         "ppm. Drift becomes drift_ppm + thermal_ppm * "
                         "sin(2π·t/period). Default 0 (disabled).")
    ap.add_argument("--thermal-period", type=float, default=120.0,
                    help="Period of thermal drift oscillation in seconds. "
                         "Default 120 s (matches typical sound-card "
                         "thermal time constant).")
    ap.add_argument("--tx-clip", type=float, default=None,
                    help="Override the channel sim's TX hard-clip threshold "
                         "(default from nbfm_channel_sim = 0.55). Pass 0 to "
                         "disable clipping entirely; useful to isolate "
                         "channel-nonlinearity effects from AWGN.")
    ap.add_argument("--phase-walks", nargs="+", type=float, default=[0.0],
                    help="Audio-domain random-walk phase noise levels "
                         "(rad/sqrt(s)). Cartesian-product with --if-noises. "
                         "0.05=FTX-1+soundcard, 0.15=SDRplay LO. Default [0.0].")
    ap.add_argument("--ota-ir-from-signature", type=str, default=None,
                    help="Path to a sounder signature.json. Sim convolves "
                         "the audio output with the OTA-measured channel "
                         "impulse response (replaces post-LPF model). "
                         "Calibrates the sim against the actual radio chain "
                         "as measured by the channel sounder.")
    ap.add_argument("--multipath", type=str, default=None,
                    help="IF-level RF multipath JSON list of "
                         "[delay_s, amp_dB, phase_rad]. First = direct path "
                         "(typ. [0,0,0]). Example to model a -10 dBc echo "
                         "at 5 ms: --multipath '[[0,0,0],[5e-3,-10,0.7]]'.")
    args = ap.parse_args()

    if not os.path.exists(args.nbfm_modem):
        print(f"ERROR: nbfm-modem binary not found at {args.nbfm_modem}", file=sys.stderr)
        print(f"  Build with: cd rust && cargo build -p modem-cli --release",
              file=sys.stderr)
        return 1

    ota_ir = None
    if args.ota_ir_from_signature:
        ota_ir = load_ir_from_signature(args.ota_ir_from_signature)
        print(f"OTA IR loaded from {args.ota_ir_from_signature}: "
              f"{len(ota_ir)} samples ({len(ota_ir)/AUDIO_RATE*1000:.1f} ms)")

    multipath = None
    if args.multipath:
        import json as _json
        try:
            multipath = [tuple(p) for p in _json.loads(args.multipath)]
            print(f"Multipath ({len(multipath)} paths): {multipath}")
        except (ValueError, TypeError) as e:
            print(f"ERROR: --multipath parse failed: {e}", file=sys.stderr)
            return 1

    os.makedirs(args.out_dir, exist_ok=True)
    rng = np.random.RandomState(args.seed)
    payload = rng.bytes(args.payload_bytes)
    payload_path = os.path.join(args.out_dir, "payload.bin")
    with open(payload_path, "wb") as f:
        f.write(payload)
    print(f"Payload: {args.payload_bytes} bytes, seed=0x{args.seed:X} → {payload_path}")
    print(f"Profiles: {args.profiles}")
    drift_desc = f"drift={args.drift_ppm:+.1f} ppm"
    if args.thermal_ppm > 0:
        drift_desc += (f" + thermal ±{args.thermal_ppm:.1f} ppm "
                       f"/ {args.thermal_period:.0f} s")
    print(f"IF noises: {args.if_noises}  ({drift_desc})")

    csv_path = os.path.join(args.out_dir, "results.csv")
    with open(csv_path, "w") as csv_f:
        csv_f.write(
            "profile,if_noise,phase_walk,sigma2,snr_est_db,sigma2_radial,"
            "sigma2_tangential,ratio_rt,converged,total,cycles,"
            "decoded_bytes,ber_byte,exact,tx_time_s,total_time_s\n")

        # Pretty-printed live table. σ²_R/σ²_T split appended on V4 RX
        # (V3 only emits the scalar σ²; the split columns stay blank
        # there).
        header = (f"{'profile':<10} {'if_n':>5} {'phw':>5} {'σ²':>8} {'SNRdB':>6} "
                  f"{'σ²_R':>7} {'σ²_T':>7} {'R/T':>5} "
                  f"{'conv':>9} {'cyc':>4} {'rx_B':>6} {'BER':>9} "
                  f"{'exact':>5} {'t(s)':>5}")
        print("\n" + header)
        print("-" * len(header))

        for profile in args.profiles:
            safe = profile.replace("+", "p")
            tx_wav = os.path.join(args.out_dir, f"tx_{safe}.wav")
            tx_time = run_tx(args.nbfm_modem, profile, payload_path, tx_wav,
                             family=args.family)
            tx_audio = read_wav_f32(tx_wav)
            print(f"\n>>> TX {profile}: {len(tx_audio)} samples "
                  f"({len(tx_audio) / AUDIO_RATE:.2f} s, build {tx_time:.1f}s)")

            for if_noise in args.if_noises:
              for phase_walk in args.phase_walks:
                t0 = time.time()
                sim_kwargs = dict(
                    if_noise_voltage=if_noise,
                    drift_ppm=args.drift_ppm,
                    thermal_ppm=args.thermal_ppm,
                    thermal_period_s=args.thermal_period,
                    phase_walk_rad_per_sqrt_s=phase_walk,
                    start_delay_s=0.5,
                    rng_seed=args.seed + int(if_noise * 1000)
                              + int(phase_walk * 10000),
                    verbose=False,
                )
                if ota_ir is not None:
                    sim_kwargs["ota_ir"] = ota_ir
                    sim_kwargs["post_lpf"] = 0.0
                if multipath is not None:
                    sim_kwargs["multipath_paths"] = multipath
                if args.tx_clip is not None:
                    sim_kwargs["tx_hard_clip"] = args.tx_clip
                ch_audio = simulate(tx_audio, **sim_kwargs)
                tag = f"if{if_noise:.2f}_pw{phase_walk:.3f}"
                ch_wav = os.path.join(args.out_dir, f"ch_{safe}_{tag}.wav")
                write_wav_f32(ch_wav, ch_audio.astype(np.float32))

                rx_bin = os.path.join(args.out_dir, f"rx_{safe}_{tag}.bin")
                info = run_rx(args.nbfm_modem, profile, ch_wav, rx_bin,
                              family=args.family)
                ber, exact = measure_ber(payload, rx_bin)
                sigma2 = info["sigma2"]
                if sigma2 is not None and sigma2 > 0:
                    snr_est = -10.0 * np.log10(sigma2)
                else:
                    snr_est = float("nan")
                t_total = time.time() - t0

                csv_f.write(
                    f"{profile},{if_noise},{phase_walk},"
                    f"{sigma2 if sigma2 is not None else ''},"
                    f"{snr_est},"
                    f"{info['sigma2_radial'] if info['sigma2_radial'] is not None else ''},"
                    f"{info['sigma2_tangential'] if info['sigma2_tangential'] is not None else ''},"
                    f"{info['ratio_rt'] if info['ratio_rt'] is not None else ''},"
                    f"{info['converged'] if info['converged'] is not None else ''},"
                    f"{info['total'] if info['total'] is not None else ''},"
                    f"{info['cycles'] if info['cycles'] is not None else ''},"
                    f"{info['decoded_bytes'] if info['decoded_bytes'] is not None else ''},"
                    f"{ber},{int(exact)},{tx_time},{t_total}\n")
                csv_f.flush()

                def fmt(v, w, p=None):
                    if v is None or (isinstance(v, float) and np.isnan(v)):
                        return f"{'-':>{w}}"
                    if p is not None:
                        return f"{v:>{w}.{p}f}"
                    return f"{v:>{w}}"

                print(
                    f"{profile:<10} "
                    f"{if_noise:>5.2f} "
                    f"{phase_walk:>5.2f} "
                    f"{fmt(sigma2, 8, 4)} "
                    f"{fmt(snr_est, 6, 1)} "
                    f"{fmt(info['sigma2_radial'], 7, 4)} "
                    f"{fmt(info['sigma2_tangential'], 7, 4)} "
                    f"{fmt(info['ratio_rt'], 5, 2)} "
                    f"{str(info['converged'] or '-'):>4}/"
                    f"{str(info['total'] or '-'):<4} "
                    f"{str(info['cycles'] or '-'):>4} "
                    f"{str(info['decoded_bytes'] or '-'):>6} "
                    f"{ber:>9.2e} "
                    f"{'Y' if exact else 'N':>5} "
                    f"{t_total:>5.1f}"
                )

                if not args.keep_wavs:
                    for p in (ch_wav, rx_bin):
                        if os.path.exists(p):
                            os.remove(p)

            if not args.keep_wavs and os.path.exists(tx_wav):
                os.remove(tx_wav)

    print(f"\nResults CSV: {csv_path}")
    print(f"  CSV columns: profile,if_noise,sigma2,snr_est_db,"
          f"sigma2_radial,sigma2_tangential,ratio_rt,converged,total,"
          f"cycles,decoded_bytes,ber_byte,exact,tx_time_s,total_time_s")
    print(f"  ratio_rt = sigma2_radial / sigma2_tangential. "
          f"On pure AWGN ≈ 1.")
    print(f"  > 2 ⇒ AM-AM/AM-PM compression-heavy; < 0.5 ⇒ phase-noise-heavy.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
