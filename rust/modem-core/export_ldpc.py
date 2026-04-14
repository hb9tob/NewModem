#!/usr/bin/env python3
"""
Exporte les matrices LDPC WiMAX (commpy) en format binaire compact pour Rust.

Format de sortie (binaire) :
  u32 little-endian : k (info bits)
  u32 little-endian : n (codeword bits)
  Pour chaque ligne de la matrice parity (n-k lignes, chacune de k bits) :
    bits packed MSB first, ((k+7)/8) octets par ligne
"""

import os, sys, struct
from commpy.channelcoding import ldpc

LDPC_DIR = os.path.join(os.path.dirname(ldpc.__file__),
                         "designs", "ldpc", "wimax")
OUT_DIR = os.path.join(os.path.dirname(__file__), "src", "ldpc_data")
os.makedirs(OUT_DIR, exist_ok=True)

CODES = {
    "wimax_960_720": "960.720.a.txt",  # rate 3/4
    "wimax_1440_720": "1440.720.txt",  # rate 1/2
}


def encode_ref(info_bits, p):
    """Reference: triang_ldpc_systematic_encode."""
    import numpy as np
    cw = ldpc.triang_ldpc_systematic_encode(
        info_bits.reshape(-1, 1), p, pad=False).flatten()
    return cw[:p["n_vnodes"]]


def extract_parity_matrix(p):
    """
    Retourne une matrice parity P (n-k, k) telle que parity_bits = (info @ P.T) mod 2.
    Trouve P en encodant la matrice identite : pour chaque info de poids 1,
    le codeword donne la colonne correspondante de [I_k | P^T].
    """
    import numpy as np
    n = p["n_vnodes"]
    k = n - p["n_cnodes"]
    P = np.zeros((n - k, k), dtype=np.uint8)
    for j in range(k):
        info = np.zeros(k, dtype=int)
        info[j] = 1
        cw = encode_ref(info, p)
        # cw = [info | parity], donc parity = cw[k:]
        # mais si cw n'est pas exactement [info | parity] (ordre different),
        # on ne peut pas extraire P comme ca. Verifier.
        # Pour WiMAX commpy : cw n'est PAS systematique direct ; il faut
        # une autre approche : reorganiser via les colonnes "info".
        P[:, j] = cw[k:]
    # Verification : ce n'est valide que si encode est systematique [info|parity]
    # Si non, on devra resoudre H * cw = 0 pour les bits parity.
    return P


def main():
    import numpy as np
    summary = []
    for name, fname in CODES.items():
        path = os.path.join(LDPC_DIR, fname)
        p = ldpc.get_ldpc_code_params(path, compute_matrix=True)
        n = p["n_vnodes"]
        k = n - p["n_cnodes"]
        print(f"{name}: n={n}, k={k}")

        # Verification que l'encodage commpy est systematique
        # Test sur une info aleatoire : les premiers k bits du cw doivent etre l'info
        rng = np.random.RandomState(0)
        info = rng.randint(0, 2, k)
        cw = encode_ref(info, p)
        if np.all(cw[:k] == info):
            print(f"  -> systematique [info|parity] OK")
            P = extract_parity_matrix(p)
            # Save
            out_path = os.path.join(OUT_DIR, f"{name}.bin")
            with open(out_path, "wb") as f:
                f.write(struct.pack("<II", k, n))
                # Pack chaque ligne MSB first
                bytes_per_row = (k + 7) // 8
                for row in P:
                    packed = bytearray(bytes_per_row)
                    for j in range(k):
                        if row[j]:
                            packed[j // 8] |= (1 << (7 - (j % 8)))
                    f.write(packed)
            print(f"  -> {out_path} ({os.path.getsize(out_path)} bytes)")

            # Sanity check : ré-encode et compare
            parity_check = np.zeros(n - k, dtype=np.uint8)
            for i in range(n - k):
                acc = 0
                for j in range(k):
                    if P[i, j] and info[j]:
                        acc ^= 1
                parity_check[i] = acc
            assert np.all(parity_check == cw[k:]), "self-check parity"
            print(f"  -> self-check OK")
            summary.append((name, k, n))
        else:
            print(f"  -> NON systematique direct, cw[:k] != info")

    # Imprime le bout de Rust pour reference
    print("\n// Rust constants:")
    for name, k, n in summary:
        print(f"pub const {name.upper()}_K: usize = {k};")
        print(f"pub const {name.upper()}_N: usize = {n};")


if __name__ == "__main__":
    main()
