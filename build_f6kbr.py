# -*- coding: utf-8 -*-
"""Assemble presentation_f6kbr.html from the HB9LC deck.

Strategy: split the existing deck into per-slide blocks, reuse their exact
HTML, insert a new accessible "primer" part + a canonical TX/RX part, push
the deep-dive chapters into an annex, and rebrand HB9LC -> F6KBR.
"""
import re
import math

SRC = "presentation_hb9lc.html"
OUT = "presentation_f6kbr.html"

html = open(SRC, encoding="utf-8").read()

# --- split into blocks -----------------------------------------------------
starts = [m.start() for m in re.finditer(r'<section class="slide', html)]
deck_close = html.index("</div> <!-- /deck -->")
head = html[: starts[0]]
tail = html[deck_close:]
blocks = []
for k, s in enumerate(starts):
    e = starts[k + 1] if k + 1 < len(starts) else deck_close
    blocks.append(html[s:e])
assert len(blocks) == 68, f"expected 68 blocks, got {len(blocks)}"

# ---------------------------------------------------------------------------
# SVG helpers (generate path data so we don't hand-place coordinates)
# ---------------------------------------------------------------------------
def sine_path(x0, y0, w, amp, cycles, phase=0.0, n=160):
    pts = []
    for i in range(n + 1):
        x = x0 + w * i / n
        y = y0 - amp * math.sin(2 * math.pi * cycles * i / n + phase)
        pts.append(f"{x:.1f},{y:.1f}")
    return "M " + " L ".join(pts)

def ask_path(x0, y0, w, amp, cycles, bits, n=240):
    """Amplitude-keyed carrier (OOK) following a bit pattern."""
    pts = []
    nb = len(bits)
    for i in range(n + 1):
        frac = i / n
        bit = bits[min(int(frac * nb), nb - 1)]
        a = amp if bit else amp * 0.06
        x = x0 + w * frac
        y = y0 - a * math.sin(2 * math.pi * cycles * frac)
        pts.append(f"{x:.1f},{y:.1f}")
    return "M " + " L ".join(pts)

def capacity_path(x0, y0, w, h, snr_min=-6, snr_max=26):
    cmax = math.log2(1 + 10 ** (snr_max / 10))
    pts = []
    n = 120
    for i in range(n + 1):
        snr = snr_min + (snr_max - snr_min) * i / n
        c = math.log2(1 + 10 ** (snr / 10))
        x = x0 + w * i / n
        y = y0 - h * c / cmax
        pts.append(f"{x:.1f},{y:.1f}")
    return "M " + " L ".join(pts)

def const_points(coords, cx, cy, r, color, rdot=5):
    out = []
    for (i, q) in coords:
        out.append(f'<circle cx="{cx + i * r:.1f}" cy="{cy - q * r:.1f}" r="{rdot}" fill="{color}"/>')
    return "\n        ".join(out)

# constellation coordinate sets (normalised)
def grid_qam(m):
    side = int(round(m ** 0.5))
    vals = [(-(side - 1) + 2 * k) for k in range(side)]
    pts = [(i / (side - 1), q / (side - 1)) for i in vals for q in vals]
    return pts

def psk(m):
    return [(math.cos(2 * math.pi * k / m), math.sin(2 * math.pi * k / m)) for k in range(m)]

def apsk16():
    inner = [(0.45 * math.cos(2 * math.pi * k / 4 + math.pi / 4),
              0.45 * math.sin(2 * math.pi * k / 4 + math.pi / 4)) for k in range(4)]
    outer = [(math.cos(2 * math.pi * k / 12), math.sin(2 * math.pi * k / 12)) for k in range(12)]
    return inner + outer

ACC = "#b8412a"; ACC2 = "#2a5f8a"; OK = "#2f7a3a"; WARN = "#b8893a"
INK = "#1a1a1a"; MUT = "#555"; SOFT = "#e6e2d8"

# ===========================================================================
# NEW SLIDES
# ===========================================================================

PRIMER_DIVIDER = """
<section class="slide section">
  <div class="ch-num">PARTIE 1 · NIVEAU DÉCOUVERTE</div>
  <h1>Faire voyager des bits par la radio</h1>
  <div class="sub">Les briques de base, sans équations méchantes. On pourra s'arrêter à la fin de cette partie — ou continuer sous le capot.</div>
</section>
"""

P1 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Le principe</div>
  <h2>Comment glisse-t-on du numérique dans une onde&nbsp;?</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>Une radio émet une <strong>onde</strong>&nbsp;: une sinusoïde, la «&nbsp;porteuse&nbsp;».</li>
        <li>Une sinusoïde n'a que <strong>trois réglages</strong>&nbsp;: son <strong>amplitude</strong>, sa <strong>fréquence</strong>, sa <strong>phase</strong>.</li>
        <li><strong>Moduler</strong> = faire varier l'un de ces réglages au rythme de nos bits.</li>
        <li>Le récepteur lit les variations et reconstruit la suite <span class="mono">1&nbsp;0&nbsp;1&nbsp;1…</span></li>
        <li>Mais le canal radio <strong>déforme l'onde</strong> au passage&nbsp;: échos, filtrage, bruit, écrêtage → il faudra la <strong>redresser</strong> à l'arrivée (égaliseur).</li>
        <li>Tout le jeu&nbsp;: en mettre le <strong>plus possible</strong> sans que le bruit s'en mêle.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 440 320" style="max-height:60vmin;">
        <rect x="0" y="0" width="440" height="320" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="220" y="26" text-anchor="middle" font-size="15" font-weight="700">La porteuse modulée par les bits</text>
        <text x="20" y="58" font-size="15" font-family="monospace" font-weight="700" fill="{INK}">1&nbsp;&nbsp;&nbsp;0&nbsp;&nbsp;&nbsp;1&nbsp;&nbsp;&nbsp;1</text>
        <g>
          <line x1="40" y1="70" x2="40" y2="130" stroke="{SOFT}"/>
          <line x1="140" y1="70" x2="140" y2="130" stroke="{SOFT}"/>
          <line x1="240" y1="70" x2="240" y2="130" stroke="{SOFT}"/>
          <line x1="340" y1="70" x2="340" y2="130" stroke="{SOFT}"/>
        </g>
        <path d="{ask_path(40, 100, 360, 28, 16, [1,0,1,1])}" stroke="{ACC}" stroke-width="2.4" fill="none"/>
        <text x="220" y="150" text-anchor="middle" font-size="12" fill="{MUT}">ici on joue sur l'<tspan font-weight="700" fill="{ACC}">amplitude</tspan> (marche / arrêt)</text>
        <g transform="translate(0,40)">
          <text x="60"  y="190" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">amplitude</text>
          <text x="220" y="190" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">fréquence</text>
          <text x="380" y="190" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">phase</text>
          <path d="{sine_path(20, 230, 80, 14, 3)}" stroke="{INK}" stroke-width="2" fill="none"/>
          <path d="{sine_path(20, 245, 80, 26, 3)}" stroke="{ACC}" stroke-width="2" fill="none" opacity="0.55"/>
          <path d="{sine_path(180, 230, 80, 18, 2)}" stroke="{INK}" stroke-width="2" fill="none"/>
          <path d="{sine_path(180, 230, 80, 18, 5)}" stroke="{ACC}" stroke-width="2" fill="none" opacity="0.55"/>
          <path d="{sine_path(340, 230, 80, 18, 3, 0)}" stroke="{INK}" stroke-width="2" fill="none"/>
          <path d="{sine_path(340, 230, 80, 18, 3, math.pi)}" stroke="{ACC}" stroke-width="2" fill="none" opacity="0.55"/>
        </g>
      </svg>
    </div>
  </div>
</section>
"""

P2 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · La règle du jeu (1/2)</div>
  <h2>Nyquist–Shannon&nbsp;: ne pas être trop pressé</h2>
  <div class="two-col two-col-1-2">
    <div class="col">
      <ul>
        <li><strong>Échantillonner</strong> un son de 3&nbsp;kHz&nbsp;? Il faut <strong>au moins 2&nbsp;mesures par période</strong> → 6&nbsp;000 mesures/s.</li>
        <li>Trop peu d'échantillons → <strong>repliement</strong> (la roue de chariot qui semble tourner à l'envers au cinéma).</li>
        <li>Symétriquement&nbsp;: dans une bande de largeur <span class="mono">B</span>, on case environ <strong><span class="mono">B</span> symboles/s</strong> sans qu'ils se chevauchent.</li>
        <li>Notre canal voix ≈ <strong>2,4&nbsp;kHz</strong> utile → de l'ordre de <strong>2&nbsp;000 symboles/s</strong>. Pas plus.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 480 300" style="max-height:58vmin;">
        <rect x="0" y="0" width="480" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="240" y="26" text-anchor="middle" font-size="14" font-weight="700">Assez d'échantillons → le signal est fidèle</text>
        <line x1="30" y1="110" x2="450" y2="110" stroke="{SOFT}"/>
        <path d="{sine_path(30, 110, 420, 55, 3)}" stroke="{ACC2}" stroke-width="2.4" fill="none"/>
        {''.join(f'<circle cx="{30+420*i/24:.1f}" cy="{110-55*math.sin(2*math.pi*3*i/24):.1f}" r="4.5" fill="{ACC}"/>' for i in range(25))}
        <text x="240" y="195" text-anchor="middle" font-size="14" font-weight="700" fill="{WARN}">Trop peu → on reconstruit une fausse onde</text>
        <line x1="30" y1="245" x2="450" y2="245" stroke="{SOFT}"/>
        <path d="{sine_path(30, 245, 420, 30, 3)}" stroke="{ACC2}" stroke-width="1.5" fill="none" opacity="0.35"/>
        {''.join(f'<circle cx="{30+420*i/4:.1f}" cy="{245-30*math.sin(2*math.pi*3*i/4):.1f}" r="5" fill="{WARN}"/>' for i in range(5))}
        <path d="{sine_path(30, 245, 420, 30, 0.0, 0)}" stroke="{WARN}" stroke-width="2.4" fill="none" stroke-dasharray="5 4"/>
      </svg>
    </div>
  </div>
</section>
"""

P3 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · La règle du jeu (2/2)</div>
  <h2>La limite de Shannon&nbsp;: le mur infranchissable</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>LA formule reine&nbsp;: <span class="mono">C = B · log₂(1 + S/N)</span></li>
        <li>En clair&nbsp;: le <strong>débit max</strong> (bits/s) dépend de la <strong>largeur de bande</strong> et du <strong>rapport signal/bruit</strong>.</li>
        <li>Deux leviers&nbsp;: <strong>élargir la bande</strong> (interdit&nbsp;: canal 12,5&nbsp;kHz) ou <strong>améliorer le S/N</strong>.</li>
        <li>Personne ne <strong>dépasse</strong> ce mur. On peut seulement <strong>s'en approcher</strong>…</li>
        <li>… et c'est tout l'enjeu du <strong>codage correcteur</strong> (la suite&nbsp;!).</li>
      </ul>
      <div class="body" style="font-size:1.9vmin; margin-top:1vmin; color:var(--muted);">
        ⚠️ Le mur suppose une source d'<strong>entropie maximale</strong> (du vrai aléatoire). Pour une source à <strong>faible entropie</strong> (compressible), <strong>compresser avant</strong> donne l'<em>illusion</em> de dépasser Shannon&nbsp;: en réalité on transmet <strong>moins de bits</strong>, pas plus vite.
      </div>
    </div>
    <div class="col">
      <svg viewBox="0 0 440 320" style="max-height:60vmin;">
        <rect x="0" y="0" width="440" height="320" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <line x1="55" y1="280" x2="410" y2="280" stroke="{INK}" stroke-width="1.5"/>
        <line x1="55" y1="280" x2="55" y2="30" stroke="{INK}" stroke-width="1.5"/>
        <text x="235" y="307" text-anchor="middle" font-size="13" fill="{MUT}">rapport signal/bruit (dB) →</text>
        <text x="20" y="155" font-size="13" fill="{MUT}" transform="rotate(-90 20 155)">débit possible →</text>
        <path d="{capacity_path(55, 280, 355, 250)}" stroke="{ACC}" stroke-width="3" fill="none"/>
        <text x="300" y="90" font-size="14" font-weight="700" fill="{ACC}">limite de Shannon</text>
        <g opacity="0.85">
          <circle cx="250" cy="172" r="6" fill="{ACC2}"/>
          <text x="262" y="176" font-size="12" fill="{ACC2}">on opère juste en dessous</text>
        </g>
        <rect x="55" y="30" width="355" height="250" fill="none"/>
        <path d="{capacity_path(55, 280, 355, 250)} L 410,30 L 55,30 Z" fill="{ACC}" opacity="0.06"/>
        <text x="200" y="55" font-size="13" fill="{MUT}">zone interdite</text>
      </svg>
    </div>
  </div>
</section>
"""

P4 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Moduler</div>
  <h2>Le vecteur tournant&nbsp;: l'astuce de l'ingénieur</h2>
  <div class="two-col two-col-1-2">
    <div class="col">
      <ul>
        <li>On représente la porteuse par un <strong>vecteur qui tourne</strong> (le «&nbsp;phaseur&nbsp;»).</li>
        <li>Sa <strong>longueur</strong> = amplitude, son <strong>angle</strong> = phase, sa <strong>vitesse</strong> = fréquence.</li>
        <li><span class="chip">OOK</span> on l'allume / l'éteint &nbsp;→ amplitude</li>
        <li><span class="chip">FSK</span> on change sa vitesse &nbsp;→ fréquence</li>
        <li><span class="chip">PSK</span> on saute son angle &nbsp;→ phase</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 380 340" style="max-height:62vmin;">
        <rect x="0" y="0" width="380" height="340" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <circle cx="150" cy="160" r="110" fill="none" stroke="{SOFT}" stroke-width="2"/>
        <line x1="30" y1="160" x2="270" y2="160" stroke="{SOFT}"/>
        <line x1="150" y1="40" x2="150" y2="280" stroke="{SOFT}"/>
        <text x="278" y="164" font-size="12" fill="{MUT}">I</text>
        <text x="156" y="48" font-size="12" fill="{MUT}">Q</text>
        <line x1="150" y1="160" x2="228" y2="82" stroke="{ACC}" stroke-width="3"/>
        <polygon points="228,82 216,86 222,96" fill="{ACC}"/>
        <path d="M 200 160 A 50 50 0 0 0 185 125" fill="none" stroke="{ACC2}" stroke-width="2"/>
        <text x="205" y="135" font-size="12" fill="{ACC2}">phase θ</text>
        <g class="anim" style="transform-origin:150px 160px; animation: spin 4s linear infinite;">
          <circle cx="228" cy="82" r="6" fill="{ACC}"/>
        </g>
        <text x="150" y="312" text-anchor="middle" font-size="12" fill="{MUT}">longueur = amplitude · vitesse = fréquence</text>
      </svg>
    </div>
  </div>
</section>
"""

# small constellation card generator
def const_card(x, y, label, bits, pts, color):
    body = const_points(pts, x + 60, y + 62, 38, color, rdot=4)
    return f"""
        <g>
          <rect x="{x}" y="{y}" width="120" height="124" rx="6" fill="#fff" stroke="{SOFT}" stroke-width="1.5"/>
          <line x1="{x+60}" y1="{y+18}" x2="{x+60}" y2="{y+106}" stroke="{SOFT}"/>
          <line x1="{x+18}" y1="{y+62}" x2="{x+102}" y2="{y+62}" stroke="{SOFT}"/>
          {body}
          <text x="{x+60}" y="{y+120}" text-anchor="middle" font-size="11" font-weight="700">{label}</text>
        </g>"""

P5 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Moduler</div>
  <h2>Le diagramme de constellation&nbsp;: la carte des symboles</h2>
  <div class="body" style="margin-bottom:1.5vmin;">
    On photographie le phaseur à l'instant de décision → un <strong>point</strong> dans le plan I/Q. L'ensemble des points = la <strong>constellation</strong>.
    Plus on met de points, plus on transmet de bits par symbole… mais plus le bruit les confond.
  </div>
  <div class="full-image">
    <svg viewBox="0 0 760 180" style="max-height:42vmin;">
      {const_card(20, 20, "BPSK — 1 bit", None, [(-1,0),(1,0)], ACC2)}
      {const_card(165, 20, "QPSK — 2 bits", None, psk(4), ACC2)}
      {const_card(310, 20, "8-PSK — 3 bits", None, psk(8), ACC)}
      {const_card(455, 20, "16-QAM — 4 bits", None, grid_qam(16), OK)}
      {const_card(620, 20, "16-APSK — 4 bits", None, apsk16(), WARN)}
    </svg>
  </div>
  <div class="caption">QAM = grille (optimal en linéaire) · APSK = points sur des cercles (aime les amplis saturés) · PSK = un seul cercle</div>
</section>
"""

P6 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · L'autre voie</div>
  <h2>L'OFDM&nbsp;: une nuée de petites porteuses</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>Au lieu d'<strong>une</strong> porteuse rapide&nbsp;: des <strong>centaines de petites porteuses lentes</strong> en parallèle.</li>
        <li>Chacune voit un canal quasi «&nbsp;plat&nbsp;» → égalisation facile, grande robustesse aux <strong>échos</strong>.</li>
        <li>C'est la recette du <strong>Wi-Fi, de la 4G/5G, du DAB et de la TNT</strong>.</li>
        <li>Élégant… alors pourquoi pas chez nous&nbsp;? → slide suivante.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 440 300" style="max-height:56vmin;">
        <rect x="0" y="0" width="440" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="220" y="26" text-anchor="middle" font-size="14" font-weight="700">Empilement de sous-porteuses</text>
        <line x1="40" y1="250" x2="410" y2="250" stroke="{INK}"/>
        {''.join(f'<line x1="{60+i*22}" y1="250" x2="{60+i*22}" y2="{250-70-30*math.sin(i*0.7):.0f}" stroke="{ACC}" stroke-width="6" opacity="0.85"/>' for i in range(15))}
        <text x="220" y="278" text-anchor="middle" font-size="12" fill="{MUT}">fréquence → (chaque trait = une porteuse)</text>
      </svg>
    </div>
  </div>
</section>
"""

P7 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · L'autre voie</div>
  <h2>… mais l'OFDM se sabote tout seul en NBFM</h2>
  <div class="two-col two-col-2-1">
    <div class="col">
      <ul>
        <li>Additionner des centaines de sinus → de <strong>gros pics d'amplitude</strong> aléatoires (PAPR élevé).</li>
        <li>Or le relais FM a un <strong>limiteur d'écrêtage</strong>&nbsp;: il rabote les pics → l'OFDM se détruit lui-même.</li>
        <li>La FM/PM est <strong>non&nbsp;linéaire</strong> et ajoute du <strong>bruit de phase</strong> → massacre les constellations denses.</li>
        <li>On a essayé (projet RustPIC)… ça ne passe pas le relais.</li>
        <li><strong>Conclusion&nbsp;: une seule porteuse, mono-fréquence.</strong></li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 320 320" style="max-height:60vmin;">
        <rect x="0" y="0" width="320" height="320" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <rect x="20" y="60" width="280" height="2.5" fill="{ACC}"/>
        <rect x="20" y="258" width="280" height="2.5" fill="{ACC}"/>
        <text x="160" y="48" text-anchor="middle" font-size="12" font-weight="700" fill="{ACC}">PLAFOND DU LIMITEUR</text>
        <path d="{sine_path(20, 160, 280, 36, 5)}" stroke="{ACC2}" stroke-width="2" fill="none" opacity="0.4"/>
        <path d="M 20,160 L 60,150 L 90,62 L 92,62 L 120,180 L 150,250 L 152,258 L 170,258 L 190,150 L 220,62 L 250,200 L 280,150 L 300,160" stroke="{INK}" stroke-width="2.4" fill="none"/>
        <text x="92" y="55" text-anchor="middle" font-size="20">✂️</text>
        <text x="205" y="55" text-anchor="middle" font-size="20">✂️</text>
        <text x="160" y="295" text-anchor="middle" font-size="13" fill="{MUT}">les pics dépassent → écrêtés → aïe&nbsp;!</text>
      </svg>
    </div>
  </div>
</section>
"""

P8 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Corriger les erreurs</div>
  <h2>Le FEC&nbsp;: corriger sans jamais redemander</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>En mode <strong>broadcast</strong> (un émetteur, plusieurs récepteurs), <strong>pas de voie retour</strong>&nbsp;: impossible de redemander un bout perdu (pas d'ARQ).</li>
        <li>Solution&nbsp;: envoyer de la <strong>redondance calculée</strong> → le récepteur <strong>corrige tout seul</strong>.</li>
        <li>Comme un <strong>Sudoku</strong>&nbsp;: même avec des cases manquantes, les contraintes laissent retrouver le reste.</li>
        <li>FEC = <em>Forward Error Correction</em> — on paie un peu de débit, on gagne énormément de robustesse.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 420 280" style="max-height:54vmin;">
        <rect x="0" y="0" width="420" height="280" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="210" y="28" text-anchor="middle" font-size="13" font-weight="700">message + redondance → canal → corrigé</text>
        {''.join(f'<rect x="{30+i*30}" y="60" width="26" height="26" rx="3" fill="{ACC2 if i<7 else OK}"/>' for i in range(10))}
        <text x="115" y="105" text-anchor="middle" font-size="11" fill="{MUT}">données</text>
        <text x="295" y="105" text-anchor="middle" font-size="11" fill="{OK}">parité</text>
        {''.join(f'<rect x="{30+i*30}" y="150" width="26" height="26" rx="3" fill="{"#fff" if i in (2,5) else (ACC2 if i<7 else OK)}" stroke="{ACC if i in (2,5) else SOFT}" stroke-width="{2 if i in (2,5) else 1}"/>' for i in range(10))}
        <text x="76" y="200" font-size="18" fill="{ACC}">✗</text>
        <text x="166" y="200" font-size="18" fill="{ACC}">✗</text>
        <text x="210" y="150" text-anchor="middle" font-size="11" fill="{ACC}">2 cases perdues dans le bruit</text>
        {''.join(f'<rect x="{30+i*30}" y="225" width="26" height="26" rx="3" fill="{ACC2 if i<7 else OK}"/>' for i in range(10))}
        <text x="375" y="243" font-size="16" fill="{OK}">✓</text>
      </svg>
    </div>
  </div>
</section>
"""

P9 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Corriger les erreurs</div>
  <h2>Pourquoi DEUX codes&nbsp;: LDPC <em>et</em> RaptorQ</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li><strong>Deux problèmes différents → deux codes complémentaires.</strong></li>
        <li><span class="chip">LDPC</span> au niveau <strong>bits</strong>&nbsp;: corrige les erreurs du bruit, très près de la limite de Shannon. Le bouclier de chaque bloc.</li>
        <li><span class="chip">RaptorQ</span> au niveau <strong>paquets</strong>&nbsp;: code <strong>fontaine</strong>. On fabrique une infinité de paquets de réparation&nbsp;; dès que le RX en a reçu <strong>assez</strong> (peu importe lesquels), le fichier est reconstruit.</li>
        <li>Idéal sans voie retour et en <strong>multi-salve</strong> (on réémet, le RX accumule).</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 420 300" style="max-height:58vmin;">
        <rect x="0" y="0" width="420" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <rect x="40" y="40" width="340" height="70" rx="8" fill="{OK}" opacity="0.12" stroke="{OK}" stroke-width="2"/>
        <text x="210" y="70" text-anchor="middle" font-size="14" font-weight="700" fill="{OK}">RaptorQ — paquets / fichier</text>
        <text x="210" y="92" text-anchor="middle" font-size="11" fill="{MUT}">remplace les PAGES entières manquantes</text>
        <rect x="40" y="130" width="340" height="70" rx="8" fill="{ACC2}" opacity="0.12" stroke="{ACC2}" stroke-width="2"/>
        <text x="210" y="160" text-anchor="middle" font-size="14" font-weight="700" fill="{ACC2}">LDPC — bits / symboles</text>
        <text x="210" y="182" text-anchor="middle" font-size="11" fill="{MUT}">répare les LETTRES abîmées</text>
        <rect x="40" y="220" width="340" height="50" rx="8" fill="{SOFT}"/>
        <text x="210" y="250" text-anchor="middle" font-size="13" font-weight="700" fill="{INK}">canal radio NBFM (bruyant, sans retour)</text>
      </svg>
    </div>
  </div>
</section>
"""

P10 = """
<section class="slide">
  <div class="eyebrow">Partie 1 · Avant d'émettre</div>
  <h2>Compresser l'image&nbsp;: même budget ~35&nbsp;ko, 4 codecs</h2>
  <div class="body" style="margin-bottom:1vmin;">
    Même photo réduite en <strong>Full-HD</strong>, encodée à <strong>~35&nbsp;ko</strong> par chacun. À ~2&nbsp;000&nbsp;symboles/s, chaque ko se paie en secondes d'antenne&nbsp;: l'efficacité du codec décide de la qualité reçue.
  </div>
  <div class="full-image">
    <img src="presentation_assets/f6kbr_compress_full.png" alt="comparaison JPEG / JPEG2000 / WebP / AVIF à 35 ko">
  </div>
  <div class="caption">JPEG (1992) → JPEG&nbsp;2000 (ondelettes) → WebP (VP8) → <strong>AVIF (AV1), le codec du modem</strong>. AVIF produit par <span class="mono">ravif</span> (speed&nbsp;1), les autres par référence.</div>
</section>
"""

P10B = """
<section class="slide">
  <div class="eyebrow">Partie 1 · Avant d'émettre</div>
  <h2>Le détail à la loupe (zoom 1:1, même 35&nbsp;ko)</h2>
  <div class="full-image">
    <img src="presentation_assets/f6kbr_compress_crop.png" alt="zoom 1:1 des artefacts par codec">
  </div>
  <div class="caption">À budget égal&nbsp;: JPEG montre ses <strong>blocs 8×8</strong> et son <em>ringing</em>&nbsp;; AVIF garde les contours nets. On compresse aussi les <strong>fichiers</strong> (zstd) avant transmission.</div>
</section>
"""

STOP = """
<section class="slide statement">
  <div>
    <div class="big">⏸️ On peut s'arrêter ici.</div>
    <div class="small">Pause questions.<br>La suite passe <strong style="color:var(--ink)">sous le capot</strong> — pour les curieux et les techniciens.</div>
  </div>
</section>
"""

ESSENTIAL_DIVIDER = """
<section class="slide section">
  <div class="ch-num">PARTIE 2 · SOUS LE CAPOT</div>
  <h1>La chaîne d'émission et de réception</h1>
  <div class="sub">Bloc par bloc, du fichier aux échantillons audio — et retour.</div>
</section>
"""

def chain_svg(title, stages, color):
    """Horizontal block chain. stages = list of (line1, line2)."""
    n = len(stages)
    bw, bh, gap = 150, 70, 30
    total = n * bw + (n - 1) * gap
    W = total + 60
    x0 = 30
    parts = [f'<text x="{W/2:.0f}" y="26" text-anchor="middle" font-size="16" font-weight="700">{title}</text>']
    for i, (a, b) in enumerate(stages):
        x = x0 + i * (bw + gap)
        parts.append(f'<rect x="{x}" y="50" width="{bw}" height="{bh}" rx="8" fill="{color}" opacity="0.12" stroke="{color}" stroke-width="2"/>')
        parts.append(f'<text x="{x+bw/2:.0f}" y="80" text-anchor="middle" font-size="14" font-weight="700" fill="{INK}">{a}</text>')
        if b:
            parts.append(f'<text x="{x+bw/2:.0f}" y="100" text-anchor="middle" font-size="11" fill="{MUT}">{b}</text>')
        if i < n - 1:
            ax = x + bw
            parts.append(f'<line x1="{ax}" y1="85" x2="{ax+gap}" y2="85" stroke="{color}" stroke-width="2.5"/>')
            parts.append(f'<polygon points="{ax+gap},85 {ax+gap-9},80 {ax+gap-9},90" fill="{color}"/>')
    return f'<svg viewBox="0 0 {W} 135" style="max-height:30vmin;">\n  ' + "\n  ".join(parts) + "\n</svg>"

TX_STAGES = [("Fichier", "image / data"), ("Compression", "+ RaptorQ"), ("LDPC", "FEC bits"),
             ("Mapping", "constellation"), ("RRC", "mise en forme"), ("Pilotes", "+ marqueurs"),
             ("Préambule", "+ VOX"), ("Audio 48k", "→ radio")]
RX_STAGES = [("Audio 48k", "micro / SDR"), ("Désemphase", "6 dB/oct"), ("Détection", "préambule"),
             ("NCO ?", "récup. porteuse"), ("Synchro", "horloge Gardner"), ("FFE", "égaliseur T/2"),
             ("DD-PLL", "phase"), ("Démapping", "→ bits"), ("LDPC", "décode"), ("RaptorQ", "→ fichier")]

E_TX = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Émission</div>
  <h2>La chaîne TX — du fichier au son</h2>
  <div class="full-image" style="flex:0 0 auto; margin-bottom:2vmin;">
    {chain_svg("Chaîne d'émission canonique", TX_STAGES, ACC)}
  </div>
  <div class="body">
    <ul>
      <li><strong>RaptorQ</strong> découpe le fichier en paquets et fabrique de la réparation&nbsp;; <strong>LDPC</strong> blinde chaque bloc de bits.</li>
      <li><strong>Mapping</strong> place les bits sur la constellation, <strong>RRC</strong> arrondit les transitions pour tenir dans la bande.</li>
      <li><strong>Pilotes &amp; marqueurs</strong> sont insérés pour aider le RX&nbsp;; le <strong>préambule</strong> et le <strong>VOX</strong> réveillent le relais.</li>
    </ul>
  </div>
</section>
"""

E_RX = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Réception</div>
  <h2>La chaîne RX — le chemin inverse, en plus dur</h2>
  <div class="full-image" style="flex:0 0 auto; margin-bottom:2vmin;">
    {chain_svg("Chaîne de réception canonique", RX_STAGES, ACC2)}
  </div>
  <div class="body">
    <ul>
      <li><strong>Désemphase</strong> compense la PM du transceiver&nbsp;; la <strong>détection</strong> trouve le préambule dans le bruit.</li>
      <li>Un <strong>NCO</strong> rattraperait la dérive de fréquence porteuse… <strong>mais sert-il vraiment en NBFM&nbsp;?</strong> → slide suivante.</li>
      <li><strong>Synchro horloge (Gardner)</strong> recale le rythme des symboles&nbsp;; le <strong>FFE</strong> redresse le canal, la <strong>DD-PLL</strong> rattrape la phase.</li>
      <li>Puis <strong>démapping → LDPC → RaptorQ</strong> reconstruisent les bits, les blocs, puis le fichier.</li>
    </ul>
  </div>
</section>
"""

E_NCO = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · La simplification clef</div>
  <h2>Le NCO&nbsp;: indispensable en SSB… inutile en FM</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>En <strong>SSB (USB/LSB)</strong>, le RX mélange avec un BFO. Un écart de fréquence TX/RX <strong>décale tout le spectre audio</strong> vers le haut ou vers le bas.</li>
        <li>Un 1000&nbsp;Hz devient 1100&nbsp;Hz&nbsp;: voix «&nbsp;Donald&nbsp;», symboles désalignés → il <strong>faut un NCO</strong> (ou une AFC) pour recentrer.</li>
        <li>En <strong>FM/PM</strong>, le démodulateur sort la <strong>fréquence instantanée</strong>. Un décalage de porteuse constant n'est plus qu'un <strong>terme continu (DC)</strong> ajouté&nbsp;: <strong>le spectre audio ne bouge pas&nbsp;!</strong></li>
        <li>Donc&nbsp;: pas de NCO. Un simple <strong>bloc anti-DC</strong> (passe-haut) suffit&nbsp;; l'égaliseur + la DD-PLL font le reste.</li>
        <li><strong>On retire le NCO de la chaîne&nbsp;: plus simple, plus robuste.</strong></li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 420 320" style="max-height:60vmin;">
        <rect x="0" y="0" width="420" height="320" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="210" y="24" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC}">SSB&nbsp;: l'écart décale tout l'audio</text>
        <line x1="30" y1="120" x2="400" y2="120" stroke="{INK}"/>
        <path d="M 90,120 C 110,55 160,55 180,120" fill="{ACC}" opacity="0.18" stroke="{ACC}" stroke-width="1.5"/>
        <path d="M 200,120 C 220,70 270,70 290,120" fill="{ACC}" opacity="0.4" stroke="{ACC}" stroke-width="2"/>
        <path d="M 178,40 L 300,40" stroke="{ACC}" stroke-width="2" marker-end="url(#a-nco)"/>
        <defs><marker id="a-nco" markerWidth="7" markerHeight="7" refX="7" refY="3.5" orient="auto"><polygon points="0,0 7,3.5 0,7" fill="{ACC}"/></marker></defs>
        <text x="240" y="34" text-anchor="middle" font-size="11" fill="{ACC}">+ Δf → glisse</text>
        <text x="40" y="138" font-size="11" fill="{MUT}">0 Hz</text>
        <text x="200" y="24" font-size="0"></text>
        <text x="210" y="190" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">FM&nbsp;: le spectre reste, une bosse DC apparaît</text>
        <line x1="30" y1="280" x2="400" y2="280" stroke="{INK}"/>
        <line x1="70" y1="280" x2="70" y2="210" stroke="{SOFT}" stroke-dasharray="4 3"/>
        <path d="M 150,280 C 175,205 245,205 270,280" fill="{ACC2}" opacity="0.3" stroke="{ACC2}" stroke-width="2"/>
        <rect x="58" y="225" width="24" height="55" fill="{OK}" opacity="0.5"/>
        <text x="70" y="300" text-anchor="middle" font-size="10" fill="{OK}">DC</text>
        <text x="280" y="300" text-anchor="middle" font-size="11" fill="{MUT}">audio fixe → passe-haut ôte le DC</text>
      </svg>
    </div>
  </div>
</section>
"""

FRAME_BRIDGE = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · La trame</div>
  <h2>Qui lit quoi dans la trame&nbsp;?</h2>
  <div class="body" style="margin-bottom:1.5vmin;">Chaque partie de la trame nourrit un bloc précis du RX&nbsp;:</div>
  <table class="profiles">
    <tr><th>Partie de la trame</th><th>Bloc RX qui la consomme</th><th>Rôle</th></tr>
    <tr><td>Préambule</td><td>Détection</td><td>repérer le début, caler l'amplitude</td></tr>
    <tr><td>Marqueur d'amorçage</td><td>Synchro + profil</td><td>verrouiller l'horloge, lire le profil (V4)</td></tr>
    <tr class="hi"><td>Pilotes continus</td><td>FFE + DD-PLL</td><td>entraîner l'égaliseur, suivre la phase</td></tr>
    <tr><td>Segment de données</td><td>Démapping → LDPC</td><td>les bits utiles, protégés</td></tr>
    <tr><td>Marqueurs de resync</td><td>FFE (re-calage)</td><td>rattraper une dérive en cours de salve</td></tr>
    <tr class="dim"><td>Fin de transmission (EOT)</td><td>Orchestrateur</td><td>on décode <strong>dès qu'on a assez de blocs</strong> (pas besoin d'attendre l'EOT)&nbsp;; l'EOT clôt la session et on <strong>se prépare à une nouvelle transmission</strong></td></tr>
  </table>
</section>
"""

ANNEX_DIVIDER = """
<section class="slide section">
  <div class="ch-num">ANNEXE · POUR LES CURIEUX</div>
  <h1>Les détours du projet</h1>
  <div class="sub">Tout ce qu'on a exploré en route&nbsp;: OFDM RustPIC, étude du canal, APSK/FTN, sondeur, SDR, low-power.</div>
</section>
"""

# ---------------------------------------------------------------------------
# new TOC
# ---------------------------------------------------------------------------
NEW_TOC = """
<section class="slide">
  <div class="eyebrow">Au programme</div>
  <h2>Deux niveaux + une annexe</h2>
  <div class="toc">
    <div class="toc-item"><span class="toc-num">P1</span><span class="toc-title">Bits sur une porteuse</span></div>
    <div class="toc-item"><span class="toc-num">P2</span><span class="toc-title">Chaîne TX / RX</span></div>
    <div class="toc-item"><span class="toc-num">P1</span><span class="toc-title">Nyquist–Shannon &amp; la limite de Shannon</span></div>
    <div class="toc-item"><span class="toc-num">P2</span><span class="toc-title">NCO inutile, dérive = DC</span></div>
    <div class="toc-item"><span class="toc-num">P1</span><span class="toc-title">Modulations &amp; constellations</span></div>
    <div class="toc-item"><span class="toc-num">P2</span><span class="toc-title">La trame ↔ les blocs RX</span></div>
    <div class="toc-item"><span class="toc-num">P1</span><span class="toc-title">OFDM (et pourquoi pas en NBFM)</span></div>
    <div class="toc-item"><span class="toc-num">P2</span><span class="toc-title">Grille &amp; égaliseur FFE</span></div>
    <div class="toc-item"><span class="toc-num">P1</span><span class="toc-title">FEC&nbsp;: LDPC + RaptorQ</span></div>
    <div class="toc-item"><span class="toc-num">P2</span><span class="toc-title">Constellations retenues</span></div>
    <div class="toc-item"><span class="toc-num">P1</span><span class="toc-title">Compression d'image</span></div>
    <div class="toc-item"><span class="toc-num">→</span><span class="toc-title">Projets futurs</span></div>
    <div class="toc-item"><span class="toc-num">⏸</span><span class="toc-title">Point d'arrêt&nbsp;: on peut s'arrêter ici</span></div>
    <div class="toc-item"><span class="toc-num">+</span><span class="toc-title">Annexe — pour les curieux</span></div>
  </div>
</section>
"""

# ---------------------------------------------------------------------------
# rebrand title slide (block 0) and TOC (block 1)
# ---------------------------------------------------------------------------
b = blocks  # alias

title_slide = b[0]
title_slide = title_slide.replace(
    "HB9LC — dimanche 31 mai 2026",
    "F6KBR · Radio-Club de Perpignan")
title_slide = title_slide.replace(
    "Du concept OFDM abandonné jusqu'au modem V3 multi-profil",
    "Des bases de la radio numérique jusqu'au modem V3 multi-profil")

# ---------------------------------------------------------------------------
# compose the new deck order
# ---------------------------------------------------------------------------
# relabel reused section dividers so the old "CHAPITRE NN" numbers
# (now out of order) match the new structure
def relabel(block, newlabel):
    return re.sub(r'(<div class="ch-num">)[^<]*(</div>)',
                  rf'\1{newlabel}\2', block, count=1)

for k in (33, 38, 45, 21):          # frame / grid / FFE / constellations
    b[k] = relabel(b[k], "PARTIE 2 · SOUS LE CAPOT")
b[61] = relabel(b[61], "PROJETS FUTURS")
for k in (4, 7, 12, 16, 26, 30, 48, 51, 55, 59):   # annex dividers
    b[k] = relabel(b[k], "ANNEXE · POUR LES CURIEUX")

# ---------------------------------------------------------------------------
# V4 frame edits on reused blocks
# ---------------------------------------------------------------------------
MK = "#4d83b3"; WU = "#d96b4e"

# slide 24 — full rewrite to the V4 layout (no header, marker-bootstrap, warmup after)
b[35] = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Trame V4</div>
  <h2>Trame V4&nbsp;: plus de header, tout est dans le marker</h2>
  <div class="full-image">
    <svg viewBox="0 0 1200 320" style="max-height:56vmin;">
      <rect x="20"  y="100" width="130" height="64" fill="{ACC}"/>
      <text x="85"  y="138" text-anchor="middle" fill="#fff" font-size="13" font-weight="700">Préambule</text>
      <text x="85"  y="180" text-anchor="middle" fill="{MUT}" font-size="11">256 QPSK</text>

      <rect x="155" y="100" width="160" height="64" fill="{MK}" stroke="{ACC}" stroke-width="3"/>
      <text x="235" y="132" text-anchor="middle" fill="#fff" font-size="13" font-weight="700">Marker₀</text>
      <text x="235" y="150" text-anchor="middle" fill="#fff" font-size="10">profil + flags</text>
      <text x="235" y="180" text-anchor="middle" fill="{MUT}" font-size="11">128 sym</text>

      <rect x="320" y="100" width="95"  height="64" fill="{WU}"/>
      <text x="367" y="138" text-anchor="middle" fill="#fff" font-size="13" font-weight="700">Warmup</text>
      <text x="367" y="180" text-anchor="middle" fill="{MUT}" font-size="11">32 sym (FFE)</text>

      <rect x="420" y="100" width="120" height="64" fill="{SOFT}" stroke="#999"/>
      <text x="480" y="136" text-anchor="middle" font-size="12" font-weight="700">Méta CW</text>

      <rect x="545" y="100" width="85"  height="64" fill="{MK}"/>
      <text x="587" y="136" text-anchor="middle" fill="#fff" font-size="11" font-weight="700">Marker</text>
      <rect x="635" y="100" width="210" height="64" fill="{SOFT}" stroke="#999"/>
      <text x="740" y="130" text-anchor="middle" font-size="12" font-weight="700">2 CW + pilotes</text>
      <text x="740" y="150" text-anchor="middle" font-size="10" fill="{MUT}">LDPC + RaptorQ</text>

      <rect x="850" y="100" width="85"  height="64" fill="{MK}"/>
      <text x="892" y="136" text-anchor="middle" fill="#fff" font-size="11" font-weight="700">Marker</text>
      <rect x="940" y="100" width="210" height="64" fill="{SOFT}" stroke="#999"/>
      <text x="1045" y="136" text-anchor="middle" font-size="12" font-weight="700">2 CW + pilotes</text>

      <g stroke="{ACC}" stroke-width="2" fill="none">
        <line x1="20" y1="80" x2="850" y2="80"/><line x1="20" y1="75" x2="20" y2="85"/><line x1="850" y1="75" x2="850" y2="85"/>
      </g>
      <text x="435" y="70" text-anchor="middle" font-size="13" fill="{ACC}" font-weight="700">Période ~4 s — préambule + marker₀ rejoués</text>

      <line x1="235" y1="200" x2="235" y2="225" stroke="{ACC}" stroke-width="1.5"/>
      <text x="235" y="245" text-anchor="middle" font-size="13" fill="{ACC}" font-weight="700">Plus de header&nbsp;: le marker₀ porte profile_index + flags (META / LAST / EOT)</text>
      <line x1="367" y1="200" x2="367" y2="270" stroke="{WU}" stroke-width="1.5"/>
      <text x="367" y="288" text-anchor="middle" font-size="13" fill="{WU}" font-weight="700">Warmup APRÈS le marker → profil connu AVANT d'entraîner le FFE</text>
    </svg>
  </div>
</section>
"""

# slide 25 — V4 bullets
b[36] = """
<section class="slide">
  <div class="eyebrow">Partie 2 · Trame V4</div>
  <h2>Marker d'amorçage, Golay, et warmup après-coup</h2>
  <div class="body">
    <ul>
      <li><strong>Header protocole supprimé.</strong> Le <strong>marker₀</strong> d'amorçage (128 sym) porte tout&nbsp;: un sync de 32&nbsp;symboles <em>auto-égalisant</em> + un payload <strong>Golay(24,12)&nbsp;+&nbsp;CRC8</strong> → décodable à froid, profil-agnostique.</li>
      <li><strong>profile_index</strong> (1 o) + <strong>fmt_version = 4</strong> logés dans des octets jadis réservés du marker → coût&nbsp;: <strong>0 symbole de plus</strong>.</li>
      <li><strong>Warmup LMS déplacé APRÈS le marker</strong> → entraîné sur la <strong>bonne constellation</strong> (le profil est déjà lu). Fin du bug d'auto-détection HIGH+.</li>
      <li>Le marker se <strong>répète avant chaque segment</strong> (~toutes les 2 CW) → chaque segment est <strong>auto-descriptif</strong>, ré-entrée robuste après une coupure.</li>
      <li>3 familles de préambules (A / B / C) conservées pour éviter les faux verrous entre profils.</li>
    </ul>
  </div>
</section>
"""

# slide 26 — pipeline: V3 -> V4 wording
b[37] = b[37].replace("Mise en trame V3", "Mise en trame V4")
b[37] = b[37].replace("préambule + warmup + header + pilotes", "préambule + marker₀ + warmup + pilotes")

# slide 29 — clarify the drifting clocks are the A/D & D/A converter clocks
b[40] = b[40].replace(
    "L'horloge TX et l'horloge RX dérivent — typiquement quelques dizaines de ppm",
    "Les horloges des convertisseurs <strong>A/D (RX) et D/A (TX)</strong> — les quartz des cartes son — dérivent l'une vis-à-vis de l'autre, typiquement quelques dizaines de ppm")

# slide 44 — QO-100 reframed: SSB-type shift, 2.7 kHz regulatory but linear over 500 kHz
b[63] = """
<section class="slide">
  <div class="eyebrow">Projets futurs · QO-100</div>
  <h2>Moyen terme&nbsp;: QO-100, un canal linéaire</h2>
  <div class="body">
    <ul>
      <li><strong>QO-100</strong> (Es'hail-2) est un transpondeur géostationnaire <strong>linéaire</strong>&nbsp;: pas de FM, <strong>pas de limiteur, pas de désemphase</strong>.</li>
      <li>On y accède par un simple <strong>décalage de fréquence type SSB</strong> (translation), pas une porteuse FM à démoduler.</li>
      <li>Largeur par station bridée à <strong>2,7 kHz</strong> par la réglementation — mais le transpondeur reste <strong>linéaire sur ~500 kHz</strong>.</li>
      <li><strong>Conséquence clé&nbsp;: ces 2,7 kHz sont utilisables à 100 %</strong>, linéairement → constellations <strong>denses</strong> (APSK/QAM), voire <strong>OFDM</strong>, sans l'écrêtage du relais FM.</li>
      <li>Reste à gérer&nbsp;: dérive Doppler résiduelle / OL, bruit de phase à 10 GHz, drift ppm. Toolchain Pluto prête (<code>feat/sdr-pluto</code>).</li>
    </ul>
  </div>
</section>
"""

# slide 47 — récap: trame V3 -> V4
b[66] = b[66].replace("Trame V3 avec re-synchro toutes les 4 s",
                      "Trame V4 (headerless) — re-synchro toutes les 4 s")

# closing slide
FINAL_SLIDE = """
<section class="slide statement">
  <div>
    <div class="big" style="font-size: 11vmin; color: var(--accent);">73&nbsp;!</div>
    <div class="small" style="font-size:2.6vmin; margin-top: 3vmin;">
      Merci au <strong style="color:var(--ink)">Radio-Club de Perpignan F6KBR</strong>.<br>
      <strong style="color:var(--ink)">HB9TOB</strong> · projet open source<br>
      <span class="mono" style="font-size:2vmin;">github.com/hb9tob/NewModem</span>
    </div>
  </div>
</section>
"""

order = []
# intro
order += [title_slide, NEW_TOC, b[2], b[3]]
# PART 1 - primer
order += [PRIMER_DIVIDER, P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P10B]
# stop point
order += [STOP]
# PART 2 - under the hood
order += [ESSENTIAL_DIVIDER, E_TX, E_RX, E_NCO, FRAME_BRIDGE]
order += b[33:38]            # frame (existing)
# grid: move LMS (slide 30, b[41]) to AFTER "comment on entraîne la DFE" (b[44])
order += [b[38], b[39], b[40], b[42], b[43], b[44], b[41]]
order += b[45:48]            # FFE (existing)
order += b[21:26]            # constellations retenues (existing)
# future
order += b[61:65]            # "La suite"
order += b[65:68]            # demo, récap, Questions?
order += [FINAL_SLIDE]       # closing slide
# ANNEX
order += [ANNEX_DIVIDER]
order += b[4:7]              # RustPIC OFDM
order += b[7:12]             # canal
order += b[12:16]            # APSK/FTN
order += b[16:21]            # pilotes
order += b[26:30]            # LDPC/RaptorQ detailed
order += b[30:33]            # couche applicative detailed
order += b[48:51]            # modem-2x Turbo
order += b[51:55]            # sondeur
order += b[55:59]            # SDR
order += b[59:61]            # low-power

body = "".join(order)

# ---------------------------------------------------------------------------
# rebrand head + tail
# ---------------------------------------------------------------------------
head_new = head.replace("Modem NBFM — HB9LC, mai 2026",
                        "Modem NBFM — F6KBR Perpignan")
# inject the rotating-phasor keyframes used by the primer (P4)
head_new = head_new.replace(
    "</style>",
    "@keyframes spin { from { transform: rotate(0deg); } to { transform: rotate(360deg); } }\n</style>")
tail_new = tail.replace("Modem NBFM · HB9LC 2026", "Modem NBFM · F6KBR")

out = head_new + body + tail_new
open(OUT, "w", encoding="utf-8").write(out)

# report
n_slides = out.count('<section class="slide')
print(f"wrote {OUT}: {n_slides} slides, {len(out)} bytes")
