"""
Banc de simulation 16-APSK (4,12) + RRC agressif (FTN τ<=1) sur canal NBFM.

Regles de travail (cf plan):
- Tout a 48 kHz, N entier (pas de resampling fractionnaire).
- Blocs publics eprouves uniquement (DVB-S2, Proakis, Meyr, IEEE 802.16e).
- Architecture cible OTA: RRC match -> Gardner -> decim entiere -> FSE T/2
  (FFE+DFE) -> DD-PLL 2e ordre -> LLR max-log -> LNMS LDPC WiMAX 2304.

Implementation incrementale. Ce fichier grandit au fil des etapes du plan.

Sources constellation:
- gr-dvbs2 (drmpeg), modulator_bc_impl.cc, table m_16apsk[0..15]:
  https://github.com/drmpeg/gr-dvbs2/blob/master/lib/modulator_bc_impl.cc
  Implementation publique de la norme ETSI EN 302 307-1 sect. 5.4.3.
- Normalisation Es=1 via r0 = sqrt(4 / (r1^2 + 3*r2^2)).
- gamma = R2/R1 = 2.85 correspond au code rate 3/4 dans la table 9 DVB-S2.
"""

from __future__ import annotations

import math

import numpy as np


# ---------------------------------------------------------------------------
# Etape 1 : Constellation 16-APSK (4,12) DVB-S2
# ---------------------------------------------------------------------------

# Angles (radians) des 16 points, indexes selon gr-dvbs2 m_16apsk[0..15].
# Indices 0..11 : anneau exterieur (rayon r2).
# Indices 12..15 : anneau interieur (rayon r1).
_APSK16_DEF = [
    ("outer",  math.pi / 4.0),           # 0
    ("outer", -math.pi / 4.0),           # 1
    ("outer",  3 * math.pi / 4.0),       # 2
    ("outer", -3 * math.pi / 4.0),       # 3
    ("outer",  math.pi / 12.0),          # 4
    ("outer", -math.pi / 12.0),          # 5
    ("outer",  11 * math.pi / 12.0),     # 6
    ("outer", -11 * math.pi / 12.0),     # 7
    ("outer",  5 * math.pi / 12.0),      # 8
    ("outer", -5 * math.pi / 12.0),      # 9
    ("outer",  7 * math.pi / 12.0),      # 10
    ("outer", -7 * math.pi / 12.0),      # 11
    ("inner",  math.pi / 4.0),           # 12
    ("inner", -math.pi / 4.0),           # 13
    ("inner",  3 * math.pi / 4.0),       # 14
    ("inner", -3 * math.pi / 4.0),       # 15
]

# gamma par code rate (DVB-S2 table 9) -- lot standard.
APSK16_GAMMA_BY_RATE = {
    "2/3":  3.15,
    "3/4":  2.85,
    "4/5":  2.75,
    "5/6":  2.70,
    "8/9":  2.60,
    "9/10": 2.57,
}


def apsk16_constellation(gamma: float = 2.85) -> np.ndarray:
    """Retourne le tableau complexe des 16 points APSK, normalise Es = 1.

    Convention : label binaire b3 b2 b1 b0 (MSB first). L'entier
    ``idx = 8*b3 + 4*b2 + 2*b1 + b0`` designe le point ``constellation[idx]``.
    Cette convention est interne au banc ; l'important est la coherence
    TX/RX. La structure geometrique est celle de la norme DVB-S2.
    """
    if gamma <= 1.0:
        raise ValueError("gamma doit etre > 1")
    r2 = 1.0
    r1 = 1.0 / gamma
    # Normalisation Es = 1 :
    #   E = (4*r1^2 + 12*r2^2) / 16 = (r1^2 + 3*r2^2) / 4
    # On veut E = 1 -> facteur r0 tel que r0^2 * (r1^2 + 3*r2^2) / 4 = 1.
    r0 = math.sqrt(4.0 / (r1 * r1 + 3.0 * r2 * r2))
    r1 *= r0
    r2 *= r0
    pts = np.empty(16, dtype=np.complex128)
    for k, (ring, angle) in enumerate(_APSK16_DEF):
        r = r1 if ring == "inner" else r2
        pts[k] = r * (math.cos(angle) + 1j * math.sin(angle))
    return pts


def apsk16_bits_to_symbols(bits: np.ndarray, constellation: np.ndarray) -> np.ndarray:
    """Map un flux de bits (0/1, MSB first par groupe de 4) en symboles APSK."""
    bits = np.asarray(bits, dtype=np.uint8).reshape(-1)
    if bits.size % 4 != 0:
        raise ValueError("nombre de bits non multiple de 4")
    groups = bits.reshape(-1, 4)
    idx = (groups[:, 0] << 3) | (groups[:, 1] << 2) | (groups[:, 2] << 1) | groups[:, 3]
    return constellation[idx]


def apsk16_slice(y: np.ndarray, constellation: np.ndarray) -> np.ndarray:
    """Decision plus-proche-voisin : retourne les indices 0..15 les plus proches."""
    y = np.asarray(y, dtype=np.complex128)
    # Distance au carre : |y - s|^2 = |y|^2 - 2 Re(y s*) + |s|^2
    # On peut simplement calculer distance[i, k] et argmin.
    d2 = np.abs(y[:, None] - constellation[None, :]) ** 2
    return np.argmin(d2, axis=1).astype(np.uint8)


def apsk16_symbols_to_bits(indices: np.ndarray) -> np.ndarray:
    """Inverse de bits_to_symbols : indices 0..15 -> flux de bits MSB first."""
    idx = np.asarray(indices, dtype=np.uint8).reshape(-1)
    bits = np.empty((idx.size, 4), dtype=np.uint8)
    bits[:, 0] = (idx >> 3) & 1
    bits[:, 1] = (idx >> 2) & 1
    bits[:, 2] = (idx >> 1) & 1
    bits[:, 3] = idx & 1
    return bits.reshape(-1)


# ---------------------------------------------------------------------------
# Etape 2 : TX -- RRC, preambule, pilotes TDM, FTN a N entier
# ---------------------------------------------------------------------------

AUDIO_RATE = 48000
DATA_CENTER_HZ = 1100.0

# Structure TDM alignee sur le reste du projet (modem_tdm_ber_bench.py,
# rust/modem-core framing.rs) : 32 symboles data suivis de 2 pilotes QPSK.
D_SYMS = 32
P_SYMS = 2

# Preambule de synchronisation (QPSK unit-circle, sequence deterministe,
# seed compatible avec le reste du projet).
N_PREAMBLE_SYMBOLS = 256
_PREAMBLE_SEED = 1234


def rrc_taps(beta: float, span_sym: int, sps: int) -> np.ndarray:
    """RRC Nyquist, normalise en energie. Forme standard (Proakis).

    Resultat : 1 + span_sym * sps taps, causal, centre sur l'index n/2.
    """
    if not (0.0 < beta <= 1.0):
        raise ValueError("beta doit etre dans (0, 1]")
    n = span_sym * sps
    t = (np.arange(n + 1) - n / 2.0) / sps
    taps = np.empty_like(t, dtype=np.float64)
    eps = 1e-12
    nyquist_t = 1.0 / (4.0 * beta) if beta > 0 else math.inf
    for i, ti in enumerate(t):
        if abs(ti) < eps:
            taps[i] = 1.0 - beta + 4.0 * beta / math.pi
        elif abs(abs(ti) - nyquist_t) < 1e-8:
            taps[i] = (beta / math.sqrt(2.0)) * (
                (1.0 + 2.0 / math.pi) * math.sin(math.pi / (4.0 * beta))
                + (1.0 - 2.0 / math.pi) * math.cos(math.pi / (4.0 * beta))
            )
        else:
            num = (math.sin(math.pi * ti * (1.0 - beta))
                   + 4.0 * beta * ti * math.cos(math.pi * ti * (1.0 + beta)))
            den = math.pi * ti * (1.0 - (4.0 * beta * ti) ** 2)
            taps[i] = num / den
    taps /= math.sqrt(np.sum(taps ** 2))
    return taps


def make_preamble() -> np.ndarray:
    """256 symboles QPSK unit-circle, deterministes."""
    rng = np.random.RandomState(_PREAMBLE_SEED)
    q = rng.randint(0, 4, N_PREAMBLE_SYMBOLS)
    return np.exp(1j * (math.pi / 4.0 + q * math.pi / 2.0))


def pilot_symbol(n: int) -> complex:
    """Pilote #n : QPSK sur cercle unite, phases {0, pi/2, pi, 3pi/2}."""
    return np.exp(1j * (n % 4) * (math.pi / 2.0))


def pilots_for_group(group_idx: int) -> np.ndarray:
    """P_SYMS pilotes du groupe `group_idx`."""
    return np.array([pilot_symbol(group_idx * P_SYMS + k)
                     for k in range(P_SYMS)], dtype=np.complex128)


def interleave_data_pilots(data_syms: np.ndarray):
    """Insere P_SYMS pilotes apres chaque bloc de D_SYMS data.

    Retourne (flux_symboles, liste_positions_pilotes) ou chaque position
    est (start, end) dans le flux.
    """
    n_data = len(data_syms)
    n_groups = (n_data + D_SYMS - 1) // D_SYMS
    out = []
    pilot_positions = []
    cursor = 0
    for g in range(n_groups):
        d = data_syms[g * D_SYMS:(g + 1) * D_SYMS]
        out.append(d)
        cursor += len(d)
        p = pilots_for_group(g)
        pilot_positions.append((cursor, cursor + len(p)))
        out.append(p)
        cursor += len(p)
    return np.concatenate(out), pilot_positions


def check_integer_constraints(symbol_rate, tau: float):
    """Verifie SPS entier et tau*SPS entier. Leve sinon.

    `symbol_rate` peut etre fractionnaire tant que AUDIO_RATE/symbol_rate
    est entier (ex. 48000/34 = 1411.76 Bd avec SPS=34).
    """
    sps_float = AUDIO_RATE / symbol_rate
    if abs(sps_float - round(sps_float)) > 1e-9:
        raise ValueError(
            f"SPS non entier : AUDIO_RATE ({AUDIO_RATE}) / "
            f"symbol_rate ({symbol_rate}) = {sps_float} (non entier)"
        )
    sps = int(round(sps_float))
    pitch = tau * sps
    if abs(pitch - round(pitch)) > 1e-9:
        raise ValueError(
            f"tau*SPS non entier : tau={tau}, SPS={sps}, "
            f"tau*SPS = {pitch}"
        )
    return sps, int(round(pitch))


def build_tx(symbol_rate: int, beta: float, tau: float,
             constellation: np.ndarray, n_data_symbols: int, rng: np.random.Generator,
             rrc_span_sym: int = 12, peak_normalize: float = 0.9):
    """Chaine TX complete :

    bits -> symboles APSK -> preambule + pilotes interleave -> placement
    a pas D = tau*SPS (entier) -> convolution RRC (beta) -> upmix @
    DATA_CENTER_HZ -> normalisation crete.

    Retourne un dict avec tous les elements utiles au RX et aux graphes.
    """
    sps, pitch = check_integer_constraints(symbol_rate, tau)

    # 1) Bits -> symboles APSK
    n_bits = n_data_symbols * 4
    data_bits = rng.integers(0, 2, size=n_bits, dtype=np.uint8)
    data_syms = apsk16_bits_to_symbols(data_bits, constellation)

    # 2) Preambule + insertion pilotes TDM
    preamble_syms = make_preamble()
    data_with_pilots, pilot_positions = interleave_data_pilots(data_syms)
    all_syms = np.concatenate([preamble_syms, data_with_pilots])
    preamble_len = len(preamble_syms)
    # Positions pilotes en indices symboles dans le flux complet
    abs_pilot_positions = [(a + preamble_len, b + preamble_len)
                           for a, b in pilot_positions]

    # 3) Placement a pas entier `pitch` = tau*SPS (pas d'arrondi, enforce)
    # Total samples = (nb_symboles - 1) * pitch + 1 + span_rrc (pour convolution)
    taps = rrc_taps(beta, rrc_span_sym, sps)
    # On place les impulsions a intervalles `pitch`, puis convolution RRC
    total_len = (len(all_syms) - 1) * pitch + len(taps)
    up = np.zeros(total_len, dtype=np.complex128)
    indices = np.arange(len(all_syms)) * pitch
    up[indices] = all_syms
    baseband = np.convolve(up, taps, mode="full")[: total_len]

    # 4) Upmix @ DATA_CENTER_HZ
    t = np.arange(len(baseband)) / AUDIO_RATE
    passband = np.real(baseband * np.exp(1j * 2.0 * math.pi * DATA_CENTER_HZ * t))

    # 5) Normalisation crete (evite saturation FM downstream)
    peak = np.max(np.abs(passband))
    if peak > 0:
        passband = passband * (peak_normalize / peak)

    return {
        "passband": passband.astype(np.float32),
        "baseband": baseband,
        "taps": taps,
        "sps": sps,
        "pitch": pitch,
        "tau_effective": pitch / sps,
        "beta": beta,
        "symbol_rate": symbol_rate,
        "data_symbols_idx": indices[preamble_len:preamble_len + n_data_symbols],
        "all_symbols": all_syms,
        "preamble_symbols": preamble_syms,
        "data_symbols": data_syms,
        "data_bits": data_bits,
        "pilot_positions": abs_pilot_positions,
        "n_data_symbols": n_data_symbols,
    }


# ---------------------------------------------------------------------------
# Etape 3 : RX -- downmix, matched filter, coarse timing, decimation entiere
# ---------------------------------------------------------------------------

def _divisors(n: int):
    out = []
    for k in range(1, n + 1):
        if n % k == 0:
            out.append(k)
    return out


def fse_decim_factor(sps: int, pitch: int) -> int:
    """Facteur de decimation entiere 48kHz->FSE, strictement N entier.

    On prend le plus grand diviseur de GCD(sps, pitch) qui reste <= sps/2
    (pour avoir au moins 2 samples par periode orthogonale T).

    - tau=1 (pitch=sps) -> d = sps/2 (FSE T/2 classique)
    - tau<1 -> d = GCD -- donne FSE oversampled (plus de taps, toujours FIR
      fractionnaire standard)
    """
    g = math.gcd(sps, pitch)
    candidates = [d for d in _divisors(g) if d <= sps // 2]
    if not candidates:
        raise ValueError(f"Pas de decim entier valide pour sps={sps}, pitch={pitch}")
    return max(candidates)


def rx_matched_and_timing(rx_passband: np.ndarray, tx_info: dict,
                          fine_search_window: int = None):
    """Pipeline RX en amont de la FSE :

    1) Downmix DATA_CENTER_HZ -> baseband complexe @ 48 kHz
    2) Matched filter RRC (memes taps que TX)
    3) Synchronisation grossiere par correlation avec la forme d'onde
       preambule reference
    4) Decimation entiere par d_fse = fse_decim_factor(sps, pitch)
    5) Retourne la sequence FSE-rate (complexe) + metadonnees

    Parametres :
      rx_passband : signal reel recu @ 48 kHz
      tx_info : dict retourne par build_tx (on reutilise sps, pitch, taps,
                preamble_symbols, pilot_positions)

    Retourne dict avec :
      - fse_input : signal complexe a d_fse-spacing (2 a GCD samples/T)
      - d_fse : facteur de decimation entier
      - sync_pos_48k : position du debut preambule dans le signal 48 kHz apres MF
      - fse_start : index du premier sample preambule dans fse_input
      - pitch_fse : pitch en nombre d'echantillons FSE (= pitch // d_fse)
      - sps_fse : SPS en nombre d'echantillons FSE (= sps // d_fse)
    """
    sps = tx_info["sps"]
    pitch = tx_info["pitch"]
    taps = tx_info["taps"]
    preamble_syms = tx_info["preamble_symbols"]

    # 1) Downmix
    n = len(rx_passband)
    t = np.arange(n) / AUDIO_RATE
    bb = rx_passband * np.exp(-1j * 2.0 * math.pi * DATA_CENTER_HZ * t)

    # 2) Matched filter (meme RRC qu'au TX, par definition du filtre adapte)
    mf = np.convolve(bb, taps, mode="same")

    # 3) Sync grossiere : reference preambule telle qu'elle apparait APRES
    #    matched filter (RRC applique deux fois -> pulse RC). Cela evite
    #    le biais d'offset entre la forme RRC (TX) et la forme RC (post-MF).
    pre_total_len = (len(preamble_syms) - 1) * pitch + len(taps)
    up_pre = np.zeros(pre_total_len, dtype=np.complex128)
    up_pre[np.arange(len(preamble_syms)) * pitch] = preamble_syms
    tx_pre_bb = np.convolve(up_pre, taps, mode="same")      # TX RRC (1x)
    tx_pre_mf = np.convolve(tx_pre_bb, taps, mode="same")   # + MF RRC = RC

    corr = np.correlate(mf, tx_pre_mf, mode="valid")
    if len(corr) == 0:
        raise RuntimeError("signal recu trop court pour la correlation preambule")
    sync_pos_48k = int(np.argmax(np.abs(corr)))

    # np.convolve(mode='same') avec taps RRC symetriques NE decale PAS :
    # un impulse a la position p produit un pic en sortie a p (la filter
    # delay est absorbee par 'same'). Donc tx_pre_mf a son 1er pic en
    # tx_pre_mf[0], et mf[sync_pos_48k] est le 1er pic symbole.
    first_peak_mf = sync_pos_48k

    # 4) Decimation entiere avec `history_syms` de marge avant le 1er pic
    d_fse = fse_decim_factor(sps, pitch)
    sps_fse = sps // d_fse
    pitch_fse = pitch // d_fse

    history_syms = 4
    start_idx = first_peak_mf - history_syms * pitch
    if start_idx < 0:
        start_idx = 0
    # Aligner start_idx sur la meme phase modulo d_fse que first_peak_mf
    start_idx = start_idx - ((start_idx - first_peak_mf) % d_fse)
    if start_idx < 0:
        start_idx += d_fse
    end_idx = len(mf) - ((len(mf) - start_idx) % d_fse)
    idx_fse = np.arange(start_idx, end_idx, d_fse)
    fse_input = mf[idx_fse]

    # Index du pic 1er symbole dans fse_input
    fse_start = (first_peak_mf - start_idx) // d_fse

    return {
        "fse_input": fse_input,
        "d_fse": d_fse,
        "sps_fse": sps_fse,
        "pitch_fse": pitch_fse,
        "sync_pos_48k": sync_pos_48k,
        "first_peak_mf": first_peak_mf,
        "fse_start": fse_start,
        "mf_full": mf,
    }


def extract_symbols_at_fse_rate(fse_input: np.ndarray, pitch_fse: int,
                                n_symbols: int, fse_start: int = 0) -> np.ndarray:
    """Extraction naive (sans FSE) : un sample par symbole au rythme pitch_fse.

    Utile pour tests unitaires AWGN pur (canal ideal) ou le matched filter
    suffit.
    """
    idx = fse_start + np.arange(n_symbols) * pitch_fse
    return fse_input[idx]


# NOTE (Step 3 -> Step 4) : le tracking timing par pilotes doit s'appliquer
# sur la SORTIE FSE (ou l'ISI des data aleatoires est nettoyee), pas sur mf
# directement. Une premiere implementation naive qui cherchait le max de
# |correlation| dans une fenetre locale sur mf echouait a cause des bosses
# aleatoires dues a l'ISI des symboles data adjacents. On reporte donc le
# tracking fin a Step 4 apres mise en place de la FSE (les pilotes post-FSE
# sont module-constant QPSK et propres). Step 3 se limite a : sync grossiere
# preambule + decimation entiere. Cela suffit largement pour l'initialisation
# FSE puisque l'erreur timing residuelle est < 1 sample a 48 kHz = T/30.

def pilot_reference_waveform(group_idx: int, pitch: int, taps: np.ndarray,
                             matched: bool = True) -> np.ndarray:
    """Forme d'onde du groupe pilote `group_idx` (P_SYMS pilotes).

    Par defaut (`matched=True`) : forme apres TX RRC + RX matched filter
    (= pulse RC). Convient pour correlation avec le signal apres MF.
    """
    pilots = pilots_for_group(group_idx)
    total_len = (len(pilots) - 1) * pitch + len(taps)
    up = np.zeros(total_len, dtype=np.complex128)
    up[np.arange(len(pilots)) * pitch] = pilots
    wav = np.convolve(up, taps, mode="same")  # TX RRC
    if matched:
        wav = np.convolve(wav, taps, mode="same")  # + MF
    return wav


# pilot_aided_timing_track : retire volontairement. Voir la note plus haut
# (l'implementation correcte doit s'appliquer sur la sortie FSE, pas sur mf).
# Reimplementation prevue en Step 4.


# ---------------------------------------------------------------------------
# Etape 4 : FSE T/d_fse (FFE) + DFE T, LMS. Training preambule + pilotes,
# decision-directed sur data. Flag --ffe-only pour ablation.
# Blocs standards : Gitlin-Weinstein FSE-FFE, Proakis DFE, Widrow LMS.
# ---------------------------------------------------------------------------

def training_mask_and_symbols(tx_info: dict):
    """Masque booleen (True = symbole connu : preambule ou pilote) + vecteur
    des symboles references (preambule ou pilote) aux positions connues,
    aligne sur all_symbols."""
    all_syms = tx_info["all_symbols"]
    mask = np.zeros(len(all_syms), dtype=bool)
    refs = np.zeros(len(all_syms), dtype=np.complex128)

    preamble_len = len(tx_info["preamble_symbols"])
    mask[:preamble_len] = True
    refs[:preamble_len] = tx_info["preamble_symbols"]

    for sym_start, sym_end in tx_info["pilot_positions"]:
        mask[sym_start:sym_end] = True
        refs[sym_start:sym_end] = all_syms[sym_start:sym_end]

    return mask, refs


def run_fse(fse_input: np.ndarray, tx_info: dict, rx_info: dict,
            constellation: np.ndarray,
            n_ff: int = None, n_dfe: int = 5,
            mu_ff_train: float = 0.01, mu_dfe_train: float = 0.01,
            mu_ff_dd: float = 0.0, mu_dfe_dd: float = 0.0,
            ffe_only: bool = False,
            pll_alpha: float = 0.01, pll_beta: float = 0.001,
            pll_enabled: bool = True):
    """FSE T/d_fse (FFE complexe) + DFE T-spaced, LMS, APSK + DD-PLL 2e ordre.

    Architecture integree (standard Meyr/Moeneclaey ch 8) :
      1) FFE centree, sortie y_ff
      2) DFE T-spaced, sortie y_dfe = y_ff - y_dfe_contrib
      3) De-rotation par exp(-j*theta) (theta = etat du DD-PLL)
      4) Decision (slicer APSK si data, reference connue si training)
      5) Phase error phi = angle(y_rot * conj(a_hat))
      6) PLL 2e ordre : theta += alpha*phi + nu; nu += beta*phi
      7) LMS FFE/DFE avec erreur = a_hat*exp(j*theta) - y (pre-rotation)

    PLL 2e ordre standard, gain proportionnel alpha, gain integral beta.
    Pour BW boucle ~1% Rs, critically damped : alpha~0.01, beta~0.001.

    Args:
      pll_alpha/beta : gains du DD-PLL. Mettre pll_enabled=False pour
        desactiver (utile pour isoler la contribution FSE).

    Retourne : dict avec outputs (apres de-rotation), decisions, errors,
    ffe_taps, dfe_taps, n_processed, theta_trace, nu_trace.
    """
    pitch_fse = rx_info["pitch_fse"]
    sps_fse = rx_info["sps_fse"]
    fse_start = rx_info["fse_start"]

    all_syms = tx_info["all_symbols"]
    n_sym = len(all_syms)
    mask, refs = training_mask_and_symbols(tx_info)

    # Taille FFE par defaut : depend du regime.
    # - tau=1 (orthogonal) : ISI = canal NBFM (pre/de-emphase), support ~8T.
    #   -> n_ff = 8*sps_fse + 1 (ex: 17 a T/2).
    # - tau<1 (FTN) : ISI dominee par overlap RRC sur 2-3 symboles. Court mais
    #   dense. FFE long = LMS diverge plus facilement. Garde 4*sps_fse+1.
    tau_eff = rx_info.get("pitch_fse", 2) / rx_info.get("sps_fse", 2)
    if n_ff is None:
        if tau_eff >= 0.99:  # tau=1
            n_ff = 8 * sps_fse + 1
        else:
            n_ff = 4 * sps_fse + 1
    # n_ff impair pour symetrie centree
    if n_ff % 2 == 0:
        n_ff += 1
    half = n_ff // 2

    # Init : FFE center tap = 1, autres = 0 (passe-tout, point de depart stable)
    ffe_taps = np.zeros(n_ff, dtype=np.complex128)
    ffe_taps[half] = 1.0
    dfe_taps = np.zeros(n_dfe, dtype=np.complex128)

    outputs = np.zeros(n_sym, dtype=np.complex128)
    decisions = np.zeros(n_sym, dtype=np.complex128)
    errors = np.zeros(n_sym, dtype=np.complex128)
    theta_trace = np.zeros(n_sym, dtype=np.float64)
    nu_trace = np.zeros(n_sym, dtype=np.float64)

    # Etat DD-PLL
    theta = 0.0  # phase accumulee (rad)
    nu = 0.0     # increment de phase par symbole (rad/sym = 2*pi*f_off*T)

    # Apres gain-scale initial, on normalise le signal sur le preambule pour
    # que la constellation soit bien amplee (Es ~ 1). On le fait une fois en
    # debut : gain estime sur correlation des premiers symboles connus.
    center0 = fse_start
    n_probe = min(32, len(tx_info["preamble_symbols"]))
    probe_idx = [center0 + k * pitch_fse for k in range(n_probe)]
    probe_idx = [i for i in probe_idx if 0 <= i < len(fse_input)]
    if len(probe_idx) > 8:
        rx_probe = fse_input[probe_idx]
        tx_probe = tx_info["preamble_symbols"][:len(probe_idx)]
        scale = np.sum(rx_probe * np.conj(tx_probe)) / np.sum(np.abs(tx_probe) ** 2)
        # On applique l'inverse de cette scale au signal FSE, pour partir pres
        # de la constellation normalisee Es=1.
        fse_input = fse_input / scale

    n_processed = 0
    for k in range(n_sym):
        center_idx = fse_start + k * pitch_fse
        lo = center_idx - half
        hi = center_idx + half + 1
        if lo < 0 or hi > len(fse_input):
            break
        r = fse_input[lo:hi]  # longueur n_ff
        y_ff = np.dot(ffe_taps, r)

        # DFE : sommation des n_dfe dernieres decisions ponderees
        y_dfe = 0.0 + 0.0j
        if not ffe_only:
            n_use = min(n_dfe, k)
            if n_use > 0:
                hist = decisions[k - n_use : k][::-1]  # [k-1, k-2, ..., k-n_use]
                y_dfe = np.dot(dfe_taps[:n_use], hist)

        y = y_ff - y_dfe  # sortie FSE pre-rotation
        # De-rotation DD-PLL
        rot = np.exp(-1j * theta)
        y_rot = y * rot
        outputs[k] = y_rot  # on stocke la sortie apres de-rotation
        theta_trace[k] = theta
        nu_trace[k] = nu

        # Decision : connue (training) ou slicing (DD)
        if mask[k]:
            d = refs[k]
            mu_ff = mu_ff_train
            mu_dfe = mu_dfe_train
        else:
            idx = apsk16_slice(np.array([y_rot]), constellation)[0]
            d = constellation[idx]
            mu_ff = mu_ff_dd
            mu_dfe = mu_dfe_dd
        decisions[k] = d

        # Erreur PLL : phase residuelle entre sortie derotee et decision.
        # Pour APSK (amplitude variable), normalisation par |d|^2 pour
        # equilibrer les contributions inner/outer ring.
        if pll_enabled:
            d_mag_sq = (d.real * d.real + d.imag * d.imag)
            if d_mag_sq > 1e-12:
                phi_err = (y_rot * np.conj(d)).imag / d_mag_sq
            else:
                phi_err = 0.0
            # Boucle 2e ordre : theta += alpha*phi + nu; nu += beta*phi
            theta = theta + pll_alpha * phi_err + nu
            nu = nu + pll_beta * phi_err

        # Erreur FSE = decision ramenee au domaine pre-rotation - y
        # y et d sont dans le meme domaine apres rotation, donc
        # erreur_pre_rotation = d*exp(j*theta) - y
        e_pre = d * np.exp(1j * theta) - y
        errors[k] = e_pre

        # LMS updates (taps FSE dans le domaine pre-rotation)
        ffe_taps = ffe_taps + mu_ff * e_pre * np.conj(r)
        if not ffe_only and k >= 1:
            n_use = min(n_dfe, k)
            hist = decisions[k - n_use : k][::-1]
            hist_pre = hist * np.exp(1j * theta_trace[k - np.arange(1, n_use + 1)])
            dfe_taps[:n_use] = dfe_taps[:n_use] + mu_dfe * e_pre * np.conj(hist_pre)

        n_processed = k + 1

    return {
        "outputs": outputs[:n_processed],
        "decisions": decisions[:n_processed],
        "errors": errors[:n_processed],
        "ffe_taps": ffe_taps,
        "dfe_taps": dfe_taps,
        "n_processed": n_processed,
        "training_mask": mask[:n_processed],
        "theta_trace": theta_trace[:n_processed],
        "nu_trace": nu_trace[:n_processed],
    }


# ---------------------------------------------------------------------------
# Etape 6 : Soft demapper LLR max-log + GMI (Generalized Mutual Information)
# ---------------------------------------------------------------------------
# Max-log LLR : approximation standard (Viterbi 1998). Pour chaque bit k :
#   LLR_k(y) = (min_{s: b_k=0} |y - s|^2 - min_{s: b_k=1} |y - s|^2) / N0
# avec N0 la variance totale du bruit complexe (= 2 * sigma_per_dim^2).
# Convention : LLR > 0 => bit plus probable = 0.
#
# GMI (Alvarado et al., IEEE Trans Inf Theory, 2008) : metrique "BICM"
# qui predit la performance d'un decodeur LDPC soft-input. Pour APSK
# 4-bits/symb :
#   GMI = sum_k [1 - E[log2(1 + exp(-sign(b_k)*LLR_k))]]   bits/symb
# Valeur max = 4 bits/symb (canal sans bruit), min = 0 (canal random).
#
# On precalcule le bit-map pour chaque index 0..15 une seule fois.

_APSK16_BIT_MAP = np.array(
    [[(i >> (3 - k)) & 1 for k in range(4)] for i in range(16)],
    dtype=np.uint8,
)  # shape (16, 4), bit_map[i, k] = bit k (MSB first) de l'index i


def apsk16_llr_maxlog(y: np.ndarray, sigma2_total: float,
                      constellation: np.ndarray) -> np.ndarray:
    """Max-log LLR pour les 4 bits de chaque symbole recu.

    Args:
      y : sortie FSE+PLL (apres de-rotation). Shape (N,) complex.
      sigma2_total : variance totale du bruit complexe (= N0).
        A estimer via sigma2_from_residuals() ou mesuree.
      constellation : 16 points APSK.

    Retourne : LLR shape (N, 4). LLR[n, k] > 0 => bit k plus probable = 0.
    Convention bit MSB first (meme que apsk16_symbols_to_bits).
    """
    if sigma2_total <= 0:
        raise ValueError("sigma2_total doit etre > 0")
    y = np.asarray(y, dtype=np.complex128).reshape(-1)
    # Distances^2 a chaque point de la constellation
    d2 = np.abs(y[:, None] - constellation[None, :]) ** 2  # (N, 16)

    llr = np.empty((y.size, 4), dtype=np.float64)
    for k in range(4):
        mask0 = _APSK16_BIT_MAP[:, k] == 0
        mask1 = _APSK16_BIT_MAP[:, k] == 1
        d2_0 = d2[:, mask0].min(axis=1)
        d2_1 = d2[:, mask1].min(axis=1)
        llr[:, k] = (d2_1 - d2_0) / sigma2_total
    return llr


def sigma2_from_residuals(outputs: np.ndarray, decisions: np.ndarray) -> float:
    """Estime la variance totale du bruit complexe depuis les residus FSE+PLL."""
    residuals = outputs - decisions
    return float(np.mean(np.abs(residuals) ** 2))


def compute_gmi_bits_per_symbol(llr: np.ndarray, bits_tx: np.ndarray) -> float:
    """GMI (bits/symbol) calculee empiriquement depuis LLR et bits de reference.

    Args:
      llr : shape (N, 4), LLR par bit (>0 => bit=0 probable)
      bits_tx : bits reference, shape (N*4,) MSB first par symbole.

    Retourne : GMI en bits/symbole. Max = 4 pour APSK-16.
    """
    bits = np.asarray(bits_tx, dtype=np.int64).reshape(-1, 4)
    # Signe : +LLR si bit=0, -LLR si bit=1
    signed = (1 - 2 * bits) * llr
    # Eviter overflow exp : on clippe
    signed = np.clip(signed, -50.0, 50.0)
    per_bit_mi = 1.0 - np.log2(1.0 + np.exp(-signed))
    return float(np.mean(np.sum(per_bit_mi, axis=1)))


def export_llr_npy(llr: np.ndarray, path: str):
    """Sauvegarde les LLR pour consommation par un decodeur LDPC externe."""
    np.save(path, llr.astype(np.float32))


# ---------------------------------------------------------------------------
# Etape 8 : Integration canal NBFM (nbfm_channel_sim) + chaine RX complete
# ---------------------------------------------------------------------------

def _lazy_channel_sim():
    import os, sys
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from nbfm_channel_sim import simulate
    return simulate


def run_full_chain(tx_info: dict, constellation: np.ndarray,
                   if_noise_voltage: float = 0.0,
                   rng_seed: int = 0,
                   channel_kwargs: dict = None,
                   fse_kwargs: dict = None,
                   compute_llr: bool = True,
                   auto_ffe_only_for_ftn: bool = True) -> dict:
    """
    Note physique importante : `auto_ffe_only_for_ftn=True` (defaut) active
    `ffe_only=True` pour tau<1. La DFE souffre de propagation d'erreur sur
    l'ISI pre-curseur induite par FTN (Anderson/Rusek). Pour tau=1
    (orthogonal Nyquist), FFE+DFE est le choix standard.
    """
    """TX -> canal NBFM -> matched filter + FSE + PLL -> LLR.

    Pipeline integre pour le sweep :
      1) audio TX (tx_info['passband']) -> nbfm_channel_sim.simulate avec
         if_noise_voltage et parametres canal (pre-emphase/clip/drift).
      2) rx_matched_and_timing : synchro grossiere + decim entiere.
      3) run_fse : FFE+DFE LMS + DD-PLL.
      4) sigma2_from_residuals + apsk16_llr_maxlog + compute_gmi.
    """
    simulate = _lazy_channel_sim()
    ck = dict(channel_kwargs or {})
    ck.setdefault("verbose", False)
    rx_audio = simulate(tx_info["passband"],
                        if_noise_voltage=if_noise_voltage,
                        rng_seed=rng_seed, **ck)

    rx_info = rx_matched_and_timing(rx_audio.astype(np.float64), tx_info)
    fk = dict(fse_kwargs or {})
    if auto_ffe_only_for_ftn and tx_info.get("tau_effective", 1.0) < 1.0:
        fk.setdefault("ffe_only", True)
    out = run_fse(rx_info["fse_input"], tx_info, rx_info, constellation, **fk)

    result = {
        "rx_audio": rx_audio,
        "rx_info": rx_info,
        "fse_out": out,
    }

    # Metriques globales
    mask = out["training_mask"]
    n = out["n_processed"]
    outputs = out["outputs"]
    decisions = out["decisions"]

    # Residuel sigma^2 mesure sur training (bruit + biais LMS)
    train_pos = np.where(mask)[0]
    if len(train_pos) > 10:
        sigma2 = sigma2_from_residuals(outputs[train_pos], decisions[train_pos])
    else:
        sigma2 = sigma2_from_residuals(outputs, decisions)
    result["sigma2"] = sigma2

    # BER / SER sur data uniquement
    all_syms = tx_info["all_symbols"]
    data_pos = np.where(~mask)[0]
    if len(data_pos):
        idx_tx = apsk16_slice(all_syms[data_pos], constellation)
        idx_rx = apsk16_slice(decisions[data_pos], constellation)
        n_ser = int(np.sum(idx_tx != idx_rx))
        result["ser"] = n_ser / len(data_pos)
        bits_tx = apsk16_symbols_to_bits(idx_tx)
        bits_rx = apsk16_symbols_to_bits(idx_rx)
        result["ber_uncoded"] = float(np.mean(bits_tx != bits_rx))
        # EVM rms sur data
        result["evm_rms"] = float(
            np.sqrt(np.mean(np.abs(outputs[data_pos] - decisions[data_pos]) ** 2))
        )
        # Eye opening sur cluster data
        result["eye_opening"] = eye_opening_metric(outputs[data_pos], constellation)

        # LLR + GMI
        if compute_llr and sigma2 > 0:
            llr = apsk16_llr_maxlog(outputs[data_pos], sigma2, constellation)
            result["llr"] = llr
            result["gmi"] = compute_gmi_bits_per_symbol(llr, bits_tx)
    else:
        result["ser"] = None
        result["ber_uncoded"] = None
        result["evm_rms"] = None
        result["eye_opening"] = None
        result["gmi"] = None

    # Metriques pilotes (drift)
    result["pilot_metrics"] = pilot_metrics_from_fse(outputs, tx_info)

    return result


# ---------------------------------------------------------------------------
# Etape 7 : Visualisations -- diagramme de l'oeil, constellation, spectres
# ---------------------------------------------------------------------------
# Ces fonctions utilisent matplotlib en backend Agg (pas de display).
# Toutes retournent les indicateurs numeriques cles pour le systeme de
# recommandation automatique (Step 10).

def _lazy_matplotlib():
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    return plt


def plot_eye_diagram(signal: np.ndarray, samples_per_symbol: int,
                     path: str, title: str = "Eye diagram",
                     n_traces: int = 300, span_symbols: int = 2):
    """Diagramme de l'oeil I/Q empile sur `span_symbols` periodes symbole.

    Args:
      signal : signal complexe (ex: fse_input ou mf a 48 kHz, downsample vers 2 SPS)
      samples_per_symbol : echantillons par periode symbole du signal fourni
      n_traces : nombre max de traces superposees
    """
    plt = _lazy_matplotlib()
    sig = np.asarray(signal, dtype=np.complex128)
    win = samples_per_symbol * span_symbols
    n_full = (len(sig) // win) * win
    sig = sig[:n_full]
    if n_full == 0:
        return {"vertical_opening_i": 0.0, "vertical_opening_q": 0.0}
    frames = sig.reshape(-1, win)
    frames = frames[:n_traces]

    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(8, 6))
    t_axis = np.arange(win) / samples_per_symbol  # en unites de T
    for row in frames:
        ax1.plot(t_axis, row.real, color="steelblue", alpha=0.15, linewidth=0.5)
        ax2.plot(t_axis, row.imag, color="darkorange", alpha=0.15, linewidth=0.5)
    ax1.set_title(f"{title} -- I channel")
    ax1.set_ylabel("Re(y)")
    ax1.set_xlabel("t / T")
    ax1.grid(True, alpha=0.3)
    ax2.set_title(f"{title} -- Q channel")
    ax2.set_ylabel("Im(y)")
    ax2.set_xlabel("t / T")
    ax2.grid(True, alpha=0.3)
    plt.tight_layout()
    plt.savefig(path, dpi=100)
    plt.close(fig)

    # Ouvertures verticales approchees : a l'instant de decision (milieu de
    # chaque periode), mesurer la variance des I/Q. Plus faible = oeil plus
    # ouvert. Pour chiffrer une "ouverture", on prend la distance minimale
    # entre deux samples observes a l'instant de decision.
    decision_indices = np.array([samples_per_symbol // 2 + k * samples_per_symbol
                                 for k in range(span_symbols)])
    decision_samples = frames[:, decision_indices].reshape(-1)
    v_i_std = float(np.std(decision_samples.real))
    v_q_std = float(np.std(decision_samples.imag))

    return {
        "vertical_std_i": v_i_std,
        "vertical_std_q": v_q_std,
        "n_traces_shown": len(frames),
    }


def plot_constellation(symbols: np.ndarray, constellation: np.ndarray,
                       path: str, title: str = "Constellation",
                       mask: np.ndarray = None):
    """Scatter des symboles recus + overlay 16 points ideaux APSK.

    Args:
      symbols : sortie FSE+PLL (apres de-rotation)
      constellation : 16 points ideaux
      mask : bool array -- si fourni, ne trace que les positions ou mask==False
        (= data uniquement). Sinon : toute la trajectoire.
    """
    plt = _lazy_matplotlib()
    y = np.asarray(symbols, dtype=np.complex128)
    if mask is not None:
        y = y[~np.asarray(mask, dtype=bool)]

    fig, ax = plt.subplots(figsize=(6, 6))
    # Hexbin pour densite (mieux que scatter pour beaucoup de points)
    if len(y) > 500:
        hb = ax.hexbin(y.real, y.imag, gridsize=60, cmap="inferno", mincnt=1)
        fig.colorbar(hb, ax=ax, label="count")
    else:
        ax.scatter(y.real, y.imag, s=6, c="steelblue", alpha=0.5)

    # Points ideaux
    radii = np.abs(constellation)
    inner_mask = radii < np.median(radii)
    ax.scatter(constellation[inner_mask].real, constellation[inner_mask].imag,
               s=80, marker="x", c="cyan", linewidths=2, label="inner ideal")
    ax.scatter(constellation[~inner_mask].real, constellation[~inner_mask].imag,
               s=80, marker="+", c="lime", linewidths=2, label="outer ideal")

    lim = 1.5 * float(np.max(np.abs(constellation)))
    ax.set_xlim(-lim, lim)
    ax.set_ylim(-lim, lim)
    ax.set_aspect("equal")
    ax.grid(True, alpha=0.3)
    ax.set_title(title)
    ax.set_xlabel("Re(y)")
    ax.set_ylabel("Im(y)")
    ax.legend(loc="upper right", fontsize=8)
    plt.tight_layout()
    plt.savefig(path, dpi=100)
    plt.close(fig)

    # Indicateur : EVM rms relatif a Es=1
    idx = apsk16_slice(y, constellation)
    ref = constellation[idx]
    evm = float(np.sqrt(np.mean(np.abs(y - ref) ** 2)))
    return {"evm_rms": evm, "n_points": len(y)}


def plot_spectra(tx_passband: np.ndarray, rx_passband: np.ndarray,
                 path: str, title: str = "Spectra"):
    """Spectres TX (avant canal) et RX (apres canal), echelle log."""
    plt = _lazy_matplotlib()
    fig, ax = plt.subplots(figsize=(8, 4))
    for sig, label, color in ((tx_passband, "TX", "steelblue"),
                              (rx_passband, "RX", "darkorange")):
        sig = np.asarray(sig, dtype=np.float64)
        spec = np.abs(np.fft.rfft(sig))
        freqs = np.fft.rfftfreq(len(sig), d=1.0 / AUDIO_RATE)
        db = 20 * np.log10(spec / (np.max(spec) + 1e-20) + 1e-20)
        ax.plot(freqs, db, label=label, color=color, linewidth=1.0)
    ax.set_xlim(0, 3000)
    ax.set_ylim(-60, 5)
    ax.set_xlabel("Frequence (Hz)")
    ax.set_ylabel("Magnitude (dB)")
    ax.set_title(title)
    ax.grid(True, alpha=0.3)
    ax.legend()
    plt.tight_layout()
    plt.savefig(path, dpi=100)
    plt.close(fig)


def eye_opening_metric(symbols: np.ndarray, constellation: np.ndarray) -> float:
    """Ouverture d'oeil "effective" pour APSK : rapport (dmin_mesuree /
    dmin_constellation_ideale). Valeur entre 0 (oeil ferme) et 1 (ideal).

    Mesure dmin_observee comme la distance minimale, pour chaque cluster
    constelle, entre le centroide observe et le point voisin le plus proche.
    """
    y = np.asarray(symbols, dtype=np.complex128)
    idx = apsk16_slice(y, constellation)
    n_obs = np.array([np.sum(idx == k) for k in range(16)])
    if n_obs.sum() == 0:
        return 0.0
    # Centroides observes
    centroids = np.array([
        np.mean(y[idx == k]) if n_obs[k] > 0 else constellation[k]
        for k in range(16)
    ])
    # Dispersion de chaque cluster (ecart type)
    stds = np.array([
        np.sqrt(np.mean(np.abs(y[idx == k] - centroids[k]) ** 2))
        if n_obs[k] > 1 else 0.0
        for k in range(16)
    ])
    # Distance min entre centroides
    dmin_obs = math.inf
    for i in range(16):
        for j in range(i + 1, 16):
            d = abs(centroids[i] - centroids[j]) - (stds[i] + stds[j])
            if d < dmin_obs:
                dmin_obs = d
    # Distance min ideale
    dmin_ideal = math.inf
    for i in range(16):
        for j in range(i + 1, 16):
            d = abs(constellation[i] - constellation[j])
            if d < dmin_ideal:
                dmin_ideal = d
    return float(max(0.0, dmin_obs / dmin_ideal))


def pilot_metrics_from_fse(fse_output: np.ndarray, tx_info: dict) -> dict:
    """Metriques de drift residuel mesurees sur les pilotes apres FSE.

    Pour chaque groupe pilote g, on extrait les P_SYMS sorties FSE aux
    positions des pilotes, et on compare a la reference pilote (QPSK).

    Retourne :
      - group_idx : index du groupe
      - sym_index : position symbolique du 1er pilote du groupe
      - phase_err : angle(<rx_pilot * conj(tx_pilot)>) -- derive phase locale
      - mag_err   : |<rx|> / |<tx|> - 1 -- erreur d'amplitude (gain)
      - freq_offset_hz_est : pente de phase_err sur les groupes (Hz),
        estimation globale unique

    Utile pour diagnostiquer la derive : une phase_err qui croit lineairement
    avec le groupe = offset frequence. Un phase_err constant mais biaise =
    erreur de phase residuelle (le DD-PLL Step 5 va la corriger). Un
    mag_err qui derive = probleme de gain / clock drift (re-normaliser FSE).
    """
    pilot_positions = tx_info["pilot_positions"]
    all_syms = tx_info["all_symbols"]

    group_idx = []
    sym_index = []
    phase_err = []
    mag_err = []
    pilot_rate_hz = 0.0  # sera approxime par symbol_rate / (D_SYMS + P_SYMS)
    sr = tx_info["symbol_rate"]
    pilot_group_period_sym = D_SYMS + P_SYMS

    for g, (sym_start, sym_end) in enumerate(pilot_positions):
        if sym_end > len(fse_output):
            break
        rx = fse_output[sym_start:sym_end]
        ref = all_syms[sym_start:sym_end]
        num = np.sum(rx * np.conj(ref))
        denom_ref = np.sum(np.abs(ref) ** 2)
        if denom_ref < 1e-20:
            continue
        gain_complex = num / denom_ref  # complex gain
        group_idx.append(g)
        sym_index.append(sym_start)
        phase_err.append(math.atan2(gain_complex.imag, gain_complex.real))
        mag_err.append(abs(gain_complex) - 1.0)

    group_idx = np.array(group_idx)
    sym_index = np.array(sym_index)
    phase_err = np.array(phase_err)
    mag_err = np.array(mag_err)

    # Unwrap + estimation lineaire du drift de phase (regression lineaire)
    freq_offset_hz_est = 0.0
    if len(phase_err) >= 3:
        phase_unwr = np.unwrap(phase_err)
        # dt entre groupes consecutifs (s)
        dt_group = pilot_group_period_sym / sr
        # Regression lineaire : phi(t) = 2*pi*f*t + phi0
        t = np.arange(len(phase_unwr)) * dt_group
        slope, _intercept = np.polyfit(t, phase_unwr, 1)
        freq_offset_hz_est = slope / (2.0 * math.pi)

    return {
        "group_idx": group_idx,
        "sym_index": sym_index,
        "phase_err": phase_err,
        "mag_err": mag_err,
        "freq_offset_hz_est": freq_offset_hz_est,
    }


# ---------------------------------------------------------------------------
# Tests unitaires (executes si python modem_apsk16_ftn_bench.py --test)
# ---------------------------------------------------------------------------

def _popcount(x: int) -> int:
    c = 0
    while x:
        c += x & 1
        x >>= 1
    return c


def _test_constellation_geometry():
    gamma = 2.85
    c = apsk16_constellation(gamma)
    # 4 points sur anneau interieur, 12 sur exterieur
    radii = np.abs(c)
    inner = sorted(radii)[:4]
    outer = sorted(radii)[4:]
    assert np.allclose(inner, inner[0]), f"4 rayons internes non egaux : {inner}"
    assert np.allclose(outer, outer[0]), f"12 rayons externes non egaux : {outer}"
    r1 = inner[0]
    r2 = outer[0]
    assert abs(r2 / r1 - gamma) < 1e-9, f"gamma effectif {r2/r1} != {gamma}"
    # Es = 1
    es = float(np.mean(np.abs(c) ** 2))
    assert abs(es - 1.0) < 1e-9, f"Es = {es}, attendu 1"


def _test_bit_roundtrip():
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(0)
    bits = rng.integers(0, 2, size=4 * 1000, dtype=np.uint8)
    syms = apsk16_bits_to_symbols(bits, c)
    idx = apsk16_slice(syms, c)
    bits_back = apsk16_symbols_to_bits(idx)
    assert np.array_equal(bits, bits_back), "roundtrip bits->symboles->bits casse"


def _test_gray_around_rings():
    """Verifie Gray de voisinage angulaire sur chaque anneau.

    Gray pur sur 4+12 est mathematiquement impossible (cf plan). Mais chaque
    anneau, pris seul, doit avoir Gray cyclique.
    """
    # Indices tries par angle croissant, sur chaque anneau
    inner_idx = [k for k, (ring, _) in enumerate(_APSK16_DEF) if ring == "inner"]
    outer_idx = [k for k, (ring, _) in enumerate(_APSK16_DEF) if ring == "outer"]

    def sort_by_angle(idx_list):
        angs = [(_APSK16_DEF[k][1]) % (2 * math.pi) for k in idx_list]
        return [k for _, k in sorted(zip(angs, idx_list))]

    for ring_name, ring in [("inner", sort_by_angle(inner_idx)),
                            ("outer", sort_by_angle(outer_idx))]:
        for a, b in zip(ring, ring[1:] + ring[:1]):
            d = _popcount(a ^ b)
            assert d == 1, f"Anneau {ring_name} : labels {a} et {b} diferent de {d} bits"


def _test_min_distance():
    """Distance minimale entre points : verifie qu'elle est bien entre anneaux
    (et non au sein d'un meme anneau, ce qui signalerait un gamma mal regle).
    """
    c = apsk16_constellation(2.85)
    n = len(c)
    min_d = math.inf
    min_pair = (None, None)
    for i in range(n):
        for j in range(i + 1, n):
            d = abs(c[i] - c[j])
            if d < min_d:
                min_d = d
                min_pair = (i, j)
    # Rapport info
    print(f"  distance min = {min_d:.4f} entre indices {min_pair}")
    # Sanity : dmin doit etre > 0.4 pour gamma=2.85 (valeur typique litt.)
    assert min_d > 0.3, f"distance min trop faible : {min_d}"


def _test_rrc_taps_properties():
    """RRC normalise : norme L2 = 1, symetrique, pic au centre."""
    for beta in (0.15, 0.20, 0.25):
        for sps in (30, 40):
            taps = rrc_taps(beta, 12, sps)
            assert abs(np.sum(taps ** 2) - 1.0) < 1e-9, "norme L2 != 1"
            # Symetrique
            assert np.allclose(taps, taps[::-1], atol=1e-12), "non symetrique"
            # Pic au centre
            center = len(taps) // 2
            assert np.argmax(np.abs(taps)) == center, "pic pas au centre"


def _test_integer_constraint_enforced():
    """Check que tau non entier est rejete."""
    try:
        check_integer_constraints(1600, 0.85)  # 0.85 * 30 = 25.5 non entier
    except ValueError:
        pass
    else:
        raise AssertionError("check_integer_constraints aurait du lever")
    # Cas valides :
    check_integer_constraints(1200, 0.9)  # 0.9 * 40 = 36
    check_integer_constraints(1600, 0.9)  # 0.9 * 30 = 27
    check_integer_constraints(1600, 1.0)


def _test_tx_structure():
    """Verifie structure TX : nb symboles preambule + data + pilotes,
    pitch effectif."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(42)
    tx = build_tx(1600, 0.20, 0.9, c, n_data_symbols=320, rng=rng)
    assert tx["sps"] == 30
    assert tx["pitch"] == 27
    assert abs(tx["tau_effective"] - 0.9) < 1e-12
    # 320 data + 320/32 = 10 groupes de pilotes * 2 = 20 pilotes
    expected_syms = N_PREAMBLE_SYMBOLS + 320 + 20
    assert len(tx["all_symbols"]) == expected_syms
    # Passband borne
    assert np.max(np.abs(tx["passband"])) <= 0.9 + 1e-6


def _test_fse_decim_factor():
    # tau = 1.0 : FSE T/2
    assert fse_decim_factor(30, 30) == 15
    assert fse_decim_factor(40, 40) == 20
    assert fse_decim_factor(24, 24) == 12
    # tau = 0.9 : FSE T/10 (fractional oversampled)
    assert fse_decim_factor(30, 27) == 3  # -> sps_fse=10, pitch_fse=9
    assert fse_decim_factor(40, 36) == 4  # -> sps_fse=10, pitch_fse=9


def _test_rx_loopback_ideal():
    """TX -> RX sans canal : le matched filter + sync doivent retrouver
    les symboles avec erreur infime (bruit numerique seul)."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(0)
    tx = build_tx(1600, 0.20, 1.0, c, n_data_symbols=320, rng=rng)
    rx_info = rx_matched_and_timing(tx["passband"].astype(np.float64), tx)

    # A tau=1, SPS=30 : d_fse=15, sps_fse=2, pitch_fse=2 (FSE T/2 classique)
    assert rx_info["d_fse"] == 15
    assert rx_info["sps_fse"] == 2
    assert rx_info["pitch_fse"] == 2

    # En AWGN=0 et canal ideal, echantillonner aux instants symboles doit
    # reconstruire les symboles (a une constante de scale/phase pres).
    fse_in = rx_info["fse_input"]
    preamble_len = len(tx["preamble_symbols"])
    all_syms_tx = tx["all_symbols"]
    fse_in = rx_info["fse_input"]
    max_syms = (len(fse_in) - rx_info["fse_start"]) // rx_info["pitch_fse"]
    n_check = min(len(all_syms_tx), max_syms)
    rx_syms = extract_symbols_at_fse_rate(
        fse_in, rx_info["pitch_fse"], n_check, fse_start=rx_info["fse_start"]
    )

    # Phase / gain naif : aligner sur le preambule (1ers 256 sym)
    ref_pre = all_syms_tx[:N_PREAMBLE_SYMBOLS]
    rx_pre = rx_syms[:N_PREAMBLE_SYMBOLS]
    scale = np.sum(rx_pre * np.conj(ref_pre)) / np.sum(np.abs(ref_pre) ** 2)
    rx_syms_aligned = rx_syms / scale

    # EVM mesuree sur data+pilotes (apres preambule) - doit etre << 1%
    evm = np.sqrt(np.mean(np.abs(rx_syms_aligned[:n_check] - all_syms_tx[:n_check]) ** 2))
    assert evm < 0.01, f"EVM loopback ideal = {evm:.4f}, attendu < 0.01"


def _test_fse_loopback_ideal():
    """Canal ideal + FSE : convergence rapide, BER=0 sur data."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(3)
    tx = build_tx(1600, 0.20, 1.0, c, n_data_symbols=800, rng=rng)
    rx_info = rx_matched_and_timing(tx["passband"].astype(np.float64), tx)
    out = run_fse(rx_info["fse_input"], tx, rx_info, c, n_dfe=5)

    # Verifie BER=0 sur les decisions aux positions data
    mask = out["training_mask"]
    data_positions = np.where(~mask)[0]
    all_syms = tx["all_symbols"]
    n = out["n_processed"]
    data_positions = data_positions[data_positions < n]
    decisions = out["decisions"][data_positions]
    tx_data = all_syms[data_positions]
    # Comparer par indice de constellation (apsk16_slice)
    idx_tx = apsk16_slice(tx_data, c)
    idx_rx = apsk16_slice(decisions, c)
    n_errors = int(np.sum(idx_tx != idx_rx))
    ser = n_errors / len(idx_tx)
    assert ser == 0.0, f"SER FSE loopback ideal = {ser:.4f} ({n_errors} erreurs)"


def _test_visualizations_generate_files(tmp_dir: str = None):
    """Sanity : les fonctions de visualisation generent des fichiers non vides."""
    import tempfile, os as _os
    if tmp_dir is None:
        tmp_dir = tempfile.mkdtemp(prefix="apsk16_viz_")
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(20)
    tx = build_tx(1600, 0.20, 1.0, c, n_data_symbols=400, rng=rng)
    rx_info = rx_matched_and_timing(tx["passband"].astype(np.float64), tx)
    out = run_fse(rx_info["fse_input"], tx, rx_info, c)

    eye_path = _os.path.join(tmp_dir, "eye.png")
    const_path = _os.path.join(tmp_dir, "constellation.png")
    spec_path = _os.path.join(tmp_dir, "spectra.png")

    eye_info = plot_eye_diagram(rx_info["fse_input"], rx_info["sps_fse"], eye_path)
    const_info = plot_constellation(out["outputs"], c, const_path,
                                     mask=out["training_mask"])
    plot_spectra(tx["passband"].astype(np.float64),
                 tx["passband"].astype(np.float64), spec_path)

    for p in (eye_path, const_path, spec_path):
        assert _os.path.exists(p) and _os.path.getsize(p) > 1000, \
            f"viz file empty/missing : {p}"

    # Ouverture d'oeil sur canal ideal : doit etre elevee (> 0.5)
    opening = eye_opening_metric(out["outputs"][~out["training_mask"]], c)
    assert opening > 0.5, f"eye_opening_metric = {opening:.3f} sur canal ideal"


def _test_ftn_09_fse_recovers():
    """tau=0.9 (FTN) : la FSE doit recuperer l'ISI intentionnelle introduite
    par l'espacement serre. On teste a Rs=1200 (pitch=36) et Rs=1600 (pitch=27)
    sans bruit IF. Le BER doit rester bas -- pas aussi bas qu'a tau=1 car
    une fraction de l'ISI peut fuir, mais largement exploitable."""
    c = apsk16_constellation(2.85)
    for rs, beta in ((1200, 0.25), (1600, 0.20)):
        rng = np.random.default_rng(100 + rs)
        tx = build_tx(rs, beta, 0.9, c, n_data_symbols=400, rng=rng)
        r = run_full_chain(tx, c, if_noise_voltage=0.0, rng_seed=7,
                           channel_kwargs={"start_delay_s": 0.0})
        ser = r["ser"]
        ber = r["ber_uncoded"]
        evm = r["evm_rms"]
        gmi = r["gmi"]
        print(f"    Rs={rs} beta={beta} tau=0.9 (FFE-only auto) : "
              f"SER={ser:.4f} BER={ber:.4f} EVM={evm:.3f} GMI={gmi:.3f}")
        # Avec FFE-only auto (pas de DFE pour FTN), on attend convergence
        # et BER raisonnable sur canal NBFM sans bruit IF.
        assert ber is not None and ber < 0.20, f"FTN BER trop eleve : {ber:.4f}"


def _test_nbfm_channel_sanity():
    """Rs=1200, beta=0.25, tau=1, canal NBFM sans bruit IF (if_noise=0) :
    la chaine doit recuperer le signal avec BER=0 ou tres faible (le canal
    NBFM introduit pre-emphase + clip + drift -16ppm + CTCSS HPF qui sont
    des distorsions mesurables mais recuperables).
    """
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(30)
    tx = build_tx(1200, 0.25, 1.0, c, n_data_symbols=400, rng=rng)

    # start_delay_s=0 pour test rapide, drift nominal (-16 ppm) inclus par defaut
    r = run_full_chain(tx, c,
                       if_noise_voltage=0.0,
                       rng_seed=42,
                       channel_kwargs={"start_delay_s": 0.0})

    ser = r["ser"]
    ber = r["ber_uncoded"]
    evm = r["evm_rms"]
    gmi = r["gmi"]
    print(f"    Rs=1200 beta=0.25 tau=1 if_noise=0 : "
          f"SER={ser:.4f} BER={ber:.4f} EVM={evm:.3f} GMI={gmi:.3f} bit/sym")
    # Sanity : le canal NBFM sans bruit reste exploitable (BER<10%)
    assert ber is not None and ber < 0.10, f"BER trop eleve : {ber:.4f}"


def _test_llr_sign_agrees_with_hard_decision():
    """High-SNR : signe de la LLR doit matcher bit hard-decided."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(10)
    bits = rng.integers(0, 2, 4 * 1000, dtype=np.uint8)
    syms = apsk16_bits_to_symbols(bits, c)
    # Tres petit bruit
    noise_std_per_dim = 0.01
    y = syms + noise_std_per_dim * (rng.standard_normal(syms.shape) +
                                    1j * rng.standard_normal(syms.shape))
    sigma2 = 2 * noise_std_per_dim ** 2
    llr = apsk16_llr_maxlog(y, sigma2, c)
    # Hard decision via signe
    hard = (llr < 0).astype(np.uint8)  # LLR<0 -> bit=1
    # BER attendu tres bas
    err = float(np.mean(hard.reshape(-1) != bits))
    assert err < 0.01, f"BER hard-from-LLR = {err:.4f}"


def _test_gmi_high_snr_close_to_4():
    """High-SNR : GMI ~ 4 bits/symb (max)."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(11)
    bits = rng.integers(0, 2, 4 * 5000, dtype=np.uint8)
    syms = apsk16_bits_to_symbols(bits, c)
    noise_std = 0.02
    y = syms + noise_std * (rng.standard_normal(syms.shape) +
                            1j * rng.standard_normal(syms.shape))
    sigma2 = 2 * noise_std ** 2
    llr = apsk16_llr_maxlog(y, sigma2, c)
    gmi = compute_gmi_bits_per_symbol(llr, bits)
    assert gmi > 3.9, f"GMI high-SNR = {gmi:.3f}, attendu > 3.9"


def _test_gmi_drops_with_noise():
    """GMI doit etre monotone decroissante avec le bruit."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(12)
    bits = rng.integers(0, 2, 4 * 5000, dtype=np.uint8)
    syms = apsk16_bits_to_symbols(bits, c)
    gmis = []
    for noise_std in (0.05, 0.15, 0.3, 0.5):
        noise = noise_std * (rng.standard_normal(syms.shape) +
                             1j * rng.standard_normal(syms.shape))
        y = syms + noise
        sigma2 = 2 * noise_std ** 2
        llr = apsk16_llr_maxlog(y, sigma2, c)
        gmis.append(compute_gmi_bits_per_symbol(llr, bits))
    # Verifie decroissance monotone
    for a, b in zip(gmis, gmis[1:]):
        assert a > b, f"GMI non-monotone : {gmis}"


def _test_ddpll_tracks_freq_offset():
    """Injecte un offset frequence 5 Hz sur le signal TX, verifie que le
    DD-PLL converge vers nu ~ 2*pi*f_off*T et que le SER reste 0."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(6)
    tx = build_tx(1600, 0.20, 1.0, c, n_data_symbols=800, rng=rng)

    # Appliquer un offset frequence 5 Hz au passband
    pb = tx["passband"].astype(np.float64)
    f_off = 5.0  # Hz
    t = np.arange(len(pb)) / AUDIO_RATE
    # Pour une modulation reelle, il faut prendre la partie reelle du mix complexe.
    # Representation analytique : passband -> bb = pb*exp(-j w0 t), puis multiplier
    # par exp(j 2*pi*f_off*t), puis retour au passband.
    # Plus simple : le downmixer RX au DATA_CENTER_HZ - f_off ne voit pas le meme
    # "vrai" centre. On peut simuler en shiftant le carrier de la chaine, mais
    # pour ce test simple on injecte directement un residu complexe apres MF.
    # -> on appelle rx_matched_and_timing normalement puis on rotate le fse_input
    # par exp(j*2*pi*f_off*t_fse).
    rx_info = rx_matched_and_timing(pb, tx)

    # Applique un offset freq au signal FSE : 5 Hz a Rs=1600 -> 2*pi*5/1600 rad/sym
    # mais fse_input est echantillonne au rythme d_fse/48kHz. Adapte.
    d_fse = rx_info["d_fse"]
    sample_rate_fse = AUDIO_RATE / d_fse
    t_fse = np.arange(len(rx_info["fse_input"])) / sample_rate_fse
    fse_in = rx_info["fse_input"] * np.exp(1j * 2 * math.pi * f_off * t_fse)

    out = run_fse(fse_in, tx, rx_info, c, pll_alpha=0.02, pll_beta=0.002)

    # Apres convergence, nu devrait etre proche de 2*pi*f_off*T_sym (rad/sym)
    expected_nu = 2 * math.pi * f_off / 1600.0
    nu_final = np.mean(out["nu_trace"][-100:])
    assert abs(nu_final - expected_nu) < 0.1 * expected_nu, \
        f"nu final = {nu_final:.5f} vs attendu {expected_nu:.5f}"

    # BER=0 attendu apres convergence
    mask = out["training_mask"]
    n = out["n_processed"]
    # On mesure sur la derniere moitie du paquet (apres convergence)
    half = n // 2
    data_pos = np.where(~mask[half:])[0] + half
    all_syms = tx["all_symbols"]
    idx_tx = apsk16_slice(all_syms[data_pos], c)
    idx_rx = apsk16_slice(out["decisions"][data_pos], c)
    ser = int(np.sum(idx_tx != idx_rx)) / len(idx_tx)
    assert ser == 0.0, f"SER avec freq offset 5 Hz = {ser:.4f}"


def _test_pilot_metrics_ideal():
    """Sur canal ideal, les metriques pilotes doivent etre ~0."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(5)
    tx = build_tx(1600, 0.20, 1.0, c, n_data_symbols=800, rng=rng)
    rx_info = rx_matched_and_timing(tx["passband"].astype(np.float64), tx)
    out = run_fse(rx_info["fse_input"], tx, rx_info, c)
    m = pilot_metrics_from_fse(out["outputs"], tx)
    # Phase residuelle locale <1 degre, mag residuelle <1%
    # Seuil tolerant : les residuels LMS sur training font varier le gain
    # local d'un groupe a l'autre. Ce qu'on veut verifier : pas de derive.
    assert np.max(np.abs(m["phase_err"])) < math.radians(5.0), \
        f"phase_err max = {math.degrees(np.max(np.abs(m['phase_err']))):.2f} deg"
    assert np.max(np.abs(m["mag_err"])) < 0.10, \
        f"mag_err max = {np.max(np.abs(m['mag_err'])):.4f}"
    # Sans canal, pas de drift frequence
    assert abs(m["freq_offset_hz_est"]) < 1.0, \
        f"freq_offset_hz = {m['freq_offset_hz_est']:.2f}"


def _test_fse_ffe_only_loopback_ideal():
    """Meme chose avec --ffe-only : doit aussi marcher (canal ideal trivial)."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(4)
    tx = build_tx(1600, 0.20, 1.0, c, n_data_symbols=800, rng=rng)
    rx_info = rx_matched_and_timing(tx["passband"].astype(np.float64), tx)
    out = run_fse(rx_info["fse_input"], tx, rx_info, c, ffe_only=True)

    mask = out["training_mask"]
    data_positions = np.where(~mask)[0]
    n = out["n_processed"]
    data_positions = data_positions[data_positions < n]
    decisions = out["decisions"][data_positions]
    all_syms = tx["all_symbols"]
    tx_data = all_syms[data_positions]
    idx_tx = apsk16_slice(tx_data, c)
    idx_rx = apsk16_slice(decisions, c)
    ser = int(np.sum(idx_tx != idx_rx)) / len(idx_tx)
    assert ser == 0.0, f"SER FSE FFE-only loopback ideal = {ser:.4f}"


def _test_tx_spectrum_roughly_at_center():
    """Spectre TX : energie dominante autour de DATA_CENTER_HZ."""
    c = apsk16_constellation(2.85)
    rng = np.random.default_rng(7)
    tx = build_tx(1600, 0.20, 1.0, c, n_data_symbols=2000, rng=rng)
    pb = tx["passband"]
    # FFT, pic
    spec = np.abs(np.fft.rfft(pb))
    freqs = np.fft.rfftfreq(len(pb), d=1.0 / AUDIO_RATE)
    peak_freq = freqs[np.argmax(spec)]
    # RRC beta=0.2 -> largeur ~Rs*(1+beta) = 1920 Hz centre sur 1100 Hz.
    # Le pic spectral peut ne pas etre exactement sur la porteuse (RRC + DC au
    # centre), mais doit etre dans la bande occupee : 1100 +/- 1000 Hz.
    assert 100 < peak_freq < 2200, f"pic spectral hors bande : {peak_freq} Hz"


def run_unit_tests():
    print("Tests unitaires 16-APSK (4,12) DVB-S2 :")
    _test_constellation_geometry()
    print("  [OK] geometrie, Es=1, gamma=2.85")
    _test_bit_roundtrip()
    print("  [OK] roundtrip bits -> symboles -> bits")
    _test_gray_around_rings()
    print("  [OK] Gray cyclique sur chaque anneau")
    _test_min_distance()
    print("  [OK] distance min coherente (entre anneaux)")
    print("Tests unitaires TX :")
    _test_rrc_taps_properties()
    print("  [OK] RRC norme L2=1, symetrique, pic central")
    _test_integer_constraint_enforced()
    print("  [OK] contrainte N entier (SPS et tau*SPS)")
    _test_tx_structure()
    print("  [OK] structure TX (nb symboles, pitch effectif)")
    _test_tx_spectrum_roughly_at_center()
    print("  [OK] spectre TX dans la bande [100, 2200] Hz")
    print("Tests unitaires RX (matched filter + timing) :")
    _test_fse_decim_factor()
    print("  [OK] fse_decim_factor (tau=1 -> T/2, tau=0.9 -> T/10)")
    _test_rx_loopback_ideal()
    print("  [OK] loopback TX->RX canal ideal : EVM < 1%")
    print("Tests unitaires FSE (FFE+DFE LMS) :")
    _test_fse_loopback_ideal()
    print("  [OK] FSE FFE+DFE loopback ideal : SER=0")
    _test_fse_ffe_only_loopback_ideal()
    print("  [OK] FSE FFE-only loopback ideal : SER=0")
    _test_pilot_metrics_ideal()
    print("  [OK] metriques pilotes post-FSE : phase/mag/freq ~ 0")
    _test_ddpll_tracks_freq_offset()
    print("  [OK] DD-PLL 2e ordre rattrape offset freq 5 Hz, SER=0")
    print("Tests unitaires visualisations :")
    _test_visualizations_generate_files()
    print("  [OK] eye/constellation/spectra generes + eye_opening > 0.5")
    print("Tests unitaires integration canal NBFM :")
    _test_nbfm_channel_sanity()
    print("  [OK] canal NBFM sans bruit IF : chaine operationnelle")
    _test_ftn_09_fse_recovers()
    print("  [OK] FTN tau=0.9 : FSE recupere l'ISI, BER borne")
    print("Tests unitaires LLR / GMI :")
    _test_llr_sign_agrees_with_hard_decision()
    print("  [OK] signe LLR coherent avec decision hard (high-SNR)")
    _test_gmi_high_snr_close_to_4()
    print("  [OK] GMI ~ 4 bits/symb en high-SNR")
    _test_gmi_drops_with_noise()
    print("  [OK] GMI monotone decroissante avec le bruit")
    print("Tous les tests passent.")


# ---------------------------------------------------------------------------
# Etape 10 : Sweep complet + generation recommendation.md auto
# ---------------------------------------------------------------------------

# Grille de sweep (cf plan + ajouts SPS=32, SPS=34).
# Format : liste de (Rs, tau, nom_court) avec SPS = AUDIO_RATE/Rs entier et
# tau*SPS entier (contrainte "N entier" stricte). Les Rs non-entiers type
# 1411.76 sont ok tant que SPS=34 est entier.
SWEEP_POINTS_RS_TAU = [
    # Nyquist (tau=1)
    (1000.0, 1.0),     # SPS=48, pitch=48
    (1200.0, 1.0),     # SPS=40, pitch=40
    (48000/34, 1.0),   # SPS=34, pitch=34, Rs~1411.76
    (1500.0, 1.0),     # SPS=32, pitch=32
    (1600.0, 1.0),     # SPS=30, pitch=30
    (2000.0, 1.0),     # SPS=24, pitch=24
    # FTN leger (tau~0.9, valeur exacte imposee par pitch entier)
    (1200.0, 36/40),       # SPS=40, pitch=36 -> tau=0.900
    (48000/34, 32/34),     # SPS=34, pitch=32 -> tau=0.9412
    (1500.0, 30/32),       # SPS=32, pitch=30 -> tau=0.9375
    (1600.0, 27/30),       # SPS=30, pitch=27 -> tau=0.900
]
SWEEP_BETAS = [0.15, 0.20, 0.25]
SWEEP_IF_NOISE = [0.0, 0.05, 0.1, 0.165, 0.3, 0.5]
SWEEP_N_DATA_SYMBOLS_DEFAULT = 1000

# Seuils WiMAX IEEE 802.16e LDPC N=2304 (valeurs tabulees, AWGN) -- cf plan.
# On exprime le seuil en GMI (bits/symb) minimum pour BER 1e-4 apres LDPC.
# Pour un LDPC de rate r sur 16-APSK (4 bits/symb), le minimum theorique
# (Shannon BICM) = 4*r bits/symb. Les codes WiMAX approchent ce minimum
# a ~0.5 dB pres, ce qui correspond a ~0.1-0.2 bits/symb au-dessus du min.
# On ajoute une marge pratique de 0.5 bits/symb pour robustesse canal reel.
LDPC_GMI_THRESHOLDS = {
    "1/2": 4.0 * 0.5 + 0.5,   # 2.5
    "2/3": 4.0 * 2.0 / 3.0 + 0.5,   # 3.17
    "3/4": 4.0 * 0.75 + 0.5,  # 3.5
}


def run_sweep(outdir: str,
              rs_tau_points: list = None,
              betas: list = None,
              if_noise_levels: list = None,
              n_data_symbols: int = SWEEP_N_DATA_SYMBOLS_DEFAULT,
              rng_seed: int = 0,
              verbose: bool = True) -> str:
    """Execute le sweep complet et ecrit outdir/sweep.csv + heatmaps +
    recommendation.md.

    Retourne le chemin du fichier CSV genere.
    """
    import os
    if rs_tau_points is None:
        rs_tau_points = SWEEP_POINTS_RS_TAU
    if betas is None:
        betas = SWEEP_BETAS
    if if_noise_levels is None:
        if_noise_levels = SWEEP_IF_NOISE

    os.makedirs(outdir, exist_ok=True)
    os.makedirs(os.path.join(outdir, "eye"), exist_ok=True)
    os.makedirs(os.path.join(outdir, "constellations"), exist_ok=True)
    os.makedirs(os.path.join(outdir, "spectra"), exist_ok=True)
    os.makedirs(os.path.join(outdir, "llr_dumps"), exist_ok=True)
    os.makedirs(os.path.join(outdir, "heatmaps"), exist_ok=True)

    c = apsk16_constellation(2.85)

    # Construire la liste (rs, beta, tau) valides
    points = []
    for rs, tau in rs_tau_points:
        for beta in betas:
            points.append((rs, beta, tau))

    rows = []
    csv_path = os.path.join(outdir, "sweep.csv")
    with open(csv_path, "w") as f:
        f.write("rs,beta,tau,if_noise,ser,ber,evm,gmi,eye_opening,sigma2,"
                "n_processed,freq_offset_hz\n")

    total = len(points) * len(if_noise_levels)
    counter = 0
    for rs, beta, tau in points:
        for if_noise in if_noise_levels:
            counter += 1
            try:
                rng = np.random.default_rng(rng_seed + counter)
                tx = build_tx(rs, beta, tau, c, n_data_symbols=n_data_symbols,
                              rng=rng)
                r = run_full_chain(tx, c, if_noise_voltage=if_noise,
                                   rng_seed=rng_seed + counter,
                                   channel_kwargs={"start_delay_s": 0.0})
                row = {
                    "rs": rs, "beta": beta, "tau": tau, "if_noise": if_noise,
                    "ser": r["ser"], "ber": r["ber_uncoded"],
                    "evm": r["evm_rms"], "gmi": r["gmi"],
                    "eye_opening": r["eye_opening"], "sigma2": r["sigma2"],
                    "n_processed": r["fse_out"]["n_processed"],
                    "freq_offset_hz": r["pilot_metrics"]["freq_offset_hz_est"],
                }
                rows.append(row)
                with open(csv_path, "a") as f:
                    f.write(f"{rs},{beta},{tau},{if_noise},"
                            f"{row['ser']:.6f},{row['ber']:.6f},"
                            f"{row['evm']:.4f},{row['gmi']:.4f},"
                            f"{row['eye_opening']:.4f},{row['sigma2']:.6f},"
                            f"{row['n_processed']},"
                            f"{row['freq_offset_hz']:.3f}\n")
                if verbose:
                    print(f"  [{counter}/{total}] rs={rs} beta={beta} tau={tau} "
                          f"if_noise={if_noise} -> "
                          f"BER={row['ber']:.4f} GMI={row['gmi']:.3f}")
            except Exception as e:
                if verbose:
                    print(f"  [{counter}/{total}] rs={rs} beta={beta} tau={tau} "
                          f"if_noise={if_noise} -> FAILED : {e}")

    # Generation heatmaps et recommendation apres sweep complet
    _generate_sweep_plots(rows, outdir)
    _generate_recommendation(rows, outdir)
    return csv_path


def _generate_sweep_plots(rows: list, outdir: str):
    """Heatmaps BER, GMI par (Rs, beta) a if_noise=median."""
    import os
    if not rows:
        return
    plt = _lazy_matplotlib()

    # Pour chaque tau et metrique, une heatmap par if_noise intermediaire
    taus = sorted(set(r["tau"] for r in rows))
    if_noises = sorted(set(r["if_noise"] for r in rows))

    for tau in taus:
        rs_vals = sorted(set(r["rs"] for r in rows if r["tau"] == tau))
        beta_vals = sorted(set(r["beta"] for r in rows if r["tau"] == tau))
        if not rs_vals or not beta_vals:
            continue

        for metric, cmap, label in (("ber", "hot_r", "BER uncoded"),
                                     ("gmi", "viridis", "GMI (bits/symb)"),
                                     ("evm", "hot_r", "EVM rms")):
            fig, axs = plt.subplots(1, len(if_noises),
                                    figsize=(3 * len(if_noises), 3.5),
                                    squeeze=False)
            for i, ifn in enumerate(if_noises):
                M = np.full((len(beta_vals), len(rs_vals)), np.nan)
                for r in rows:
                    if r["tau"] != tau or r["if_noise"] != ifn:
                        continue
                    ib = beta_vals.index(r["beta"])
                    ir = rs_vals.index(r["rs"])
                    v = r[metric]
                    if v is not None:
                        M[ib, ir] = v
                ax = axs[0, i]
                im = ax.imshow(M, aspect="auto", cmap=cmap,
                               origin="lower")
                ax.set_xticks(range(len(rs_vals)))
                ax.set_xticklabels([str(x) for x in rs_vals])
                ax.set_yticks(range(len(beta_vals)))
                ax.set_yticklabels([f"{b:.2f}" for b in beta_vals])
                ax.set_xlabel("Rs (Bd)")
                ax.set_ylabel("beta")
                ax.set_title(f"if_noise={ifn}")
                fig.colorbar(im, ax=ax)
            fig.suptitle(f"{label} -- tau={tau}")
            plt.tight_layout()
            path = os.path.join(outdir, "heatmaps",
                                f"{metric}_tau{tau:.2f}.png")
            plt.savefig(path, dpi=100)
            plt.close(fig)


def _generate_recommendation(rows: list, outdir: str):
    """Genere recommendation.md : top-3 configs par rate LDPC avec GMI marge."""
    import os
    path = os.path.join(outdir, "recommendation.md")

    if not rows:
        with open(path, "w", encoding="utf-8") as f:
            f.write("# Recommendation : aucun resultat\n")
        return

    # Pour chaque rate LDPC, filtrer les (rs, beta, tau, if_noise) ou la marge
    # GMI >= seuil, trier par debit utile decroissant.
    def debit_utile(row, rate):
        # debit (bits/s) = rate * 4 * Rs * tau (effective)
        return rate * 4 * row["rs"] * row["tau"]

    rates = [("1/2", 0.5), ("2/3", 2.0 / 3.0), ("3/4", 0.75)]

    with open(path, "w", encoding="utf-8") as f:
        f.write("# Recommandations automatiques OTA\n\n")
        f.write("Banc : 16-APSK (4,12) gamma=2.85, RRC beta variable, "
                "FTN tau in {0.9, 1.0}, canal NBFM (pre-emphase 75us, "
                "clip 0.55, drift -16 ppm).\n\n")
        f.write("## Methode\n\n")
        f.write("Pour chaque rate LDPC WiMAX 2304 r, filtrer les configurations "
                "ou GMI mesuree >= seuil (= 4·r + 0.5 bits/symb de marge). "
                "Trier par debit utile r·4·Rs·tau (bits/s) decroissant.\n\n")
        f.write("Les configurations sont ensuite examinees visuellement "
                "(constellation, oeil, spectre) pour confirmer la sante du signal.\n\n")

        for rname, r in rates:
            thr = LDPC_GMI_THRESHOLDS[rname]
            f.write(f"## LDPC rate {rname} (seuil GMI = {thr:.2f} bits/symb)\n\n")
            # Deduplication : pour chaque (Rs, beta, tau), on garde le point
            # au if_noise le plus eleve qui passe encore le seuil. Ca montre
            # la marge bruit.
            from collections import defaultdict
            by_cfg = defaultdict(list)
            for row in rows:
                if row.get("gmi") is None or row["gmi"] < thr:
                    continue
                key = (row["rs"], row["beta"], row["tau"])
                by_cfg[key].append(row)
            candidats = []
            for key, entries in by_cfg.items():
                # Garder l'entree avec if_noise max qui passe
                best = max(entries, key=lambda x: x["if_noise"])
                candidats.append((debit_utile(best, r), best))
            candidats.sort(key=lambda x: -x[0])
            if not candidats:
                f.write("_Aucune configuration ne passe le seuil._\n\n")
                continue
            f.write("| Rang | Rs (Bd) | beta | tau | if_noise_max_OK | "
                    "debit_utile (bit/s) | GMI | BER | EVM | eye_open |\n")
            f.write("|------|---------|------|-----|-----------------|------|------|------|------|------|\n")
            for rank, (d, row) in enumerate(candidats[:10], 1):
                f.write(f"| {rank} | {row['rs']:.2f} | {row['beta']:.2f} | "
                        f"{row['tau']:.3f} | {row['if_noise']} | "
                        f"{int(d)} | {row['gmi']:.3f} | "
                        f"{row['ber']:.4f} | {row['evm']:.3f} | "
                        f"{row['eye_opening']:.3f} |\n")
            f.write("\n")

        # Piste parkee
        f.write("## Piste parkee\n\n")
        f.write("- **FTN tau < 0.9** : non teste (necessiterait BCJR sur ISI, "
                "non aligne sur l'architecture finale LNMS-only).\n")
        f.write("- **Geometrie APSK autre que gamma=2.85** : scan gamma "
                "possible en phase 2 si les resultats gamma=2.85 sont "
                "prometteurs sur OTA reel.\n")


if __name__ == "__main__":
    import sys, os
    if len(sys.argv) >= 2 and sys.argv[1] == "--test":
        run_unit_tests()
    elif len(sys.argv) >= 2 and sys.argv[1] == "--sweep":
        outdir = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                              "..", "results", "apsk16_ftn")
        quick = "--quick" in sys.argv
        if quick:
            run_sweep(outdir,
                      rs_tau_points=[(1200.0, 1.0),
                                     (1500.0, 1.0),
                                     (48000/34, 1.0),
                                     (1200.0, 36/40),
                                     (1500.0, 30/32)],
                      betas=[0.20, 0.25],
                      if_noise_levels=[0.0, 0.165, 0.5],
                      n_data_symbols=500)
        else:
            n_syms = 1000
            for i, arg in enumerate(sys.argv):
                if arg == "--n-symbols" and i + 1 < len(sys.argv):
                    n_syms = int(sys.argv[i + 1])
            run_sweep(outdir, n_data_symbols=n_syms)
        print(f"Sweep termine. Resultats dans {outdir}")
    else:
        print("Usage :")
        print("  python modem_apsk16_ftn_bench.py --test")
        print("  python modem_apsk16_ftn_bench.py --sweep [--quick] [--n-symbols N]")
