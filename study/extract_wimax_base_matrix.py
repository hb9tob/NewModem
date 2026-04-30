"""Extract IEEE 802.16e LDPC base matrix from RPTU alist file.

Input  : .alist file describing the expanded H matrix (M x N binary sparse).
Output : 4 x 24 base matrix in {-1, 0..z-1} where -1 = zero block, s = cyclic
         shift of identity I_z.

Convention reminder (MacKay alist):
  line 1 : N M
  line 2 : max_col_weight max_row_weight
  line 3 : col_weights[N]
  line 4 : row_weights[M]
  N lines : (1-based) row indices where each column has a 1
  M lines : (1-based) column indices where each row has a 1
"""

import sys
from pathlib import Path


def load_alist(path: Path):
    lines = path.read_text().splitlines()
    n, m = map(int, lines[0].split())
    # max weights, col weights, row weights: ignored (the full sparse
    # list that follows is sufficient).
    # (1-based) row indices where each column has a 1.
    col_lists = []
    cursor = 4  # after the 4 header lines
    for _ in range(n):
        entries = [int(x) for x in lines[cursor].split() if int(x) != 0]
        col_lists.append(entries)
        cursor += 1
    # Per-row column indices - not needed (col_lists is enough to
    # rebuild H).
    return n, m, col_lists


def expand_h(n, m, col_lists):
    """Reconstruit H[m][n] dense binaire (0/1) depuis col_lists 1-based."""
    h = [[0] * n for _ in range(m)]
    for c, rows in enumerate(col_lists):
        for r in rows:
            h[r - 1][c] = 1  # alist is 1-based
    return h


def extract_base_matrix(h, m, n, z=96, mb=4, nb=24):
    """For each (br, bc) sub-block of size z*z, determine the cyclic
    shift: -1 if zero block, s in 0..z-1 otherwise (row 0 of the
    sub-block has its single 1 at column s mod z).
    """
    assert m == mb * z, f"m={m} != mb*z={mb*z}"
    assert n == nb * z, f"n={n} != nb*z={nb*z}"
    base = [[-1] * nb for _ in range(mb)]
    for br in range(mb):
        for bc in range(nb):
            row_top = br * z  # row 0 of the sub-block
            col_off = bc * z
            shift = -1
            for j in range(z):
                if h[row_top][col_off + j] == 1:
                    shift = j
                    break
            # Consistency check: if a shift was found, verify the other
            # rows of the sub-block honour the shifted identity.
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
                # Check that the block is fully zero.
                for i in range(z):
                    for j in range(z):
                        if h[row_top + i][col_off + j] != 0:
                            raise ValueError(
                                f"Sub-block ({br},{bc}) not all-zero (entry at "
                                f"({i},{j})) but row 0 was zero - non-cyclic"
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
    print(f"alist parsed: N={n}, M={m}, K={n - m} (rate = {(n - m) / n:.4f})")
    h = expand_h(n, m, col_lists)
    # Sanity : count ones (should be sum of col weights)
    ones = sum(sum(row) for row in h)
    print(f"  total ones = {ones}")
    base = extract_base_matrix(h, m, n)
    print()
    print(format_rust(base))


if __name__ == "__main__":
    main()
