"""Extract IEEE 802.16e LDPC base matrix from RPTU alist file.

Input  : .alist file describing the expanded H matrix (M x N binary sparse).
Output : 4 x 24 base matrix in {-1, 0..z-1} where -1 = zero block, s = cyclic
         shift of identity I_z.

Convention rappel (alist MacKay) :
  ligne 1 : N M
  ligne 2 : max_col_weight max_row_weight
  ligne 3 : col_weights[N]
  ligne 4 : row_weights[M]
  N lignes : indices (1-based) des lignes où la colonne a un 1
  M lignes : indices (1-based) des colonnes où la ligne a un 1
"""

import sys
from pathlib import Path


def load_alist(path: Path):
    lines = path.read_text().splitlines()
    n, m = map(int, lines[0].split())
    # max weights, col weights, row weights : on ignore (suffisant : la
    # liste sparse complète suit).
    # Indices (1-based) des lignes où chaque colonne a un 1.
    col_lists = []
    cursor = 4  # après 4 lignes d'en-tête
    for _ in range(n):
        entries = [int(x) for x in lines[cursor].split() if int(x) != 0]
        col_lists.append(entries)
        cursor += 1
    # Indices des colonnes par ligne — on n'en a pas besoin (col_lists
    # suffit pour reconstruire H).
    return n, m, col_lists


def expand_h(n, m, col_lists):
    """Reconstruit H[m][n] dense binaire (0/1) depuis col_lists 1-based."""
    h = [[0] * n for _ in range(m)]
    for c, rows in enumerate(col_lists):
        for r in rows:
            h[r - 1][c] = 1  # alist est 1-based
    return h


def extract_base_matrix(h, m, n, z=96, mb=4, nb=24):
    """Pour chaque sous-bloc (br, bc) de taille z×z, déterminer le shift
    cyclique : -1 si bloc nul, s ∈ 0..z-1 sinon (la ligne 0 du sous-bloc
    a son 1 unique à la colonne s mod z).
    """
    assert m == mb * z, f"m={m} ≠ mb*z={mb*z}"
    assert n == nb * z, f"n={n} ≠ nb*z={nb*z}"
    base = [[-1] * nb for _ in range(mb)]
    for br in range(mb):
        for bc in range(nb):
            row_top = br * z  # ligne 0 du sous-bloc
            col_off = bc * z
            shift = -1
            for j in range(z):
                if h[row_top][col_off + j] == 1:
                    shift = j
                    break
            # Validation cohérence : si shift trouvé, vérifier que les
            # autres lignes du sous-bloc respectent l'identité décalée.
            if shift >= 0:
                for i in range(z):
                    expected_col = (i + shift) % z
                    for j in range(z):
                        v = h[row_top + i][col_off + j]
                        want = 1 if j == expected_col else 0
                        if v != want:
                            raise ValueError(
                                f"Sub-block ({br},{bc}) not a clean cyclic shift "
                                f"of identity at row {i}, col {j}: got {v}, want {want} "
                                f"(shift={shift})"
                            )
            else:
                # Vérifier que le bloc est entièrement nul.
                for i in range(z):
                    for j in range(z):
                        if h[row_top + i][col_off + j] != 0:
                            raise ValueError(
                                f"Sub-block ({br},{bc}) not all-zero (entry at "
                                f"({i},{j})) but row 0 was zero — non-cyclic"
                            )
            base[br][bc] = shift
    return base


def format_rust(base, mb=4, nb=24):
    out = ["const BASE_R5_6: [BaseEntry; 4 * 24] = ["]
    for br in range(mb):
        row = base[br]
        cells = [f"{v:>3}" for v in row]
        out.append(f"    /* row {br} */ " + ", ".join(cells) + ",")
    out.append("];")
    return "\n".join(out)


def main():
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/tmp/wimax_2304_5_6.alist")
    n, m, col_lists = load_alist(path)
    print(f"alist parsed : N={n}, M={m}, K={n - m} (rate = {(n - m) / n:.4f})")
    h = expand_h(n, m, col_lists)
    # Sanity : count ones (should be sum of col weights)
    ones = sum(sum(row) for row in h)
    print(f"  total ones = {ones}")
    base = extract_base_matrix(h, m, n)
    print()
    print(format_rust(base))


if __name__ == "__main__":
    main()
