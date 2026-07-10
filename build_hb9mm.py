# -*- coding: utf-8 -*-
"""Assemble presentation_hb9mm.html from the HB9LC deck.

Strategy: split the existing deck into per-slide blocks, reuse their exact
HTML, insert a new accessible "primer" part + a canonical TX/RX part + the
turbo-receiver deep dive (CFO / SFO / turbo loop / scrambler), push the
legacy deep-dive chapters into an annex, and rebrand HB9LC -> HB9MM.

The frame chapter is rewritten to describe ONLY the current V4 frame and the
technical reason for each part — no evolution narrative, no past mistakes.
"""
import re
import math

SRC = "presentation_hb9lc.html"
OUT = "presentation_hb9mm.html"

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
        <li><span class="chip">QAM</span> <span class="chip">APSK</span> on joue sur <strong>amplitude ET phase à la fois</strong> → beaucoup plus de points, donc plus de bits par symbole (ce que le modem utilise vraiment).</li>
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

_IMG = 'style="max-height:100%; max-width:100%; width:auto; object-fit:contain;"'
P10 = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Avant d'émettre · même budget ~35&nbsp;ko (1/2)</div>
  <h2>Compresser l'image&nbsp;: JPEG vs JPEG&nbsp;2000</h2>
  <div class="full-image">
    <img src="presentation_assets/hb9mm_compress_full_a.png" alt="JPEG et JPEG 2000 à 35 ko" {_IMG}>
  </div>
  <div class="caption">Même photo Full-HD à ~35&nbsp;ko chacun. <strong>JPEG</strong> (1992, blocs 8×8) → <strong>JPEG&nbsp;2000</strong> (ondelettes). À suivre&nbsp;: WebP et AVIF.</div>
</section>
"""

P10B = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Avant d'émettre</div>
  <h2>Compresser l'image&nbsp;: même budget ~35&nbsp;ko (2/2)</h2>
  <div class="full-image">
    <img src="presentation_assets/hb9mm_compress_full_b.png" alt="WebP et AVIF à 35 ko" {_IMG}>
  </div>
  <div class="caption"><strong>WebP</strong> (VP8) → <strong>AVIF (AV1), le codec du modem</strong> (produit par <span class="mono">ravif</span>). C'est le plus efficace à budget égal.</div>
</section>
"""

P10C = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Avant d'émettre</div>
  <h2>Le détail à la loupe — zoom 1:1 (1/2)</h2>
  <div class="full-image">
    <img src="presentation_assets/hb9mm_compress_crop_a.png" alt="zoom 1:1 JPEG / JPEG 2000" {_IMG}>
  </div>
  <div class="caption">À budget égal, <strong>JPEG</strong> montre ses <strong>blocs 8×8</strong> et son <em>ringing</em>&nbsp;; <strong>JPEG&nbsp;2000</strong> lisse mais reste flou.</div>
</section>
"""

P10D = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Avant d'émettre</div>
  <h2>Le détail à la loupe — zoom 1:1 (2/2)</h2>
  <div class="full-image">
    <img src="presentation_assets/hb9mm_compress_crop_b.png" alt="zoom 1:1 WebP / AVIF" {_IMG}>
  </div>
  <div class="caption"><strong>WebP</strong> puis <strong>AVIF</strong> gardent les contours nets. On compresse aussi les <strong>fichiers</strong> (zstd) avant transmission.</div>
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

def _wrap_label(a, width=13):
    """Split a label into at most two lines near the middle space."""
    if "\n" in a:
        return a.split("\n")
    if len(a) <= width:
        return [a]
    mid = len(a) // 2
    spaces = [i for i, ch in enumerate(a) if ch == " "]
    if not spaces:
        return [a]
    cut = min(spaces, key=lambda i: abs(i - mid))
    return [a[:cut], a[cut + 1:]]


def chain_svg(title, stages, color, per_row=5):
    """Snaking block chain with big, multi-line blocks. stages = (label, sub)."""
    n = len(stages)
    bw, bh, hgap, vgap = 205, 90, 30, 62
    x0, y0 = 30, 46
    rows = (n + per_row - 1) // per_row
    W = x0 * 2 + per_row * bw + (per_row - 1) * hgap
    H = y0 + rows * (bh + vgap)
    parts = [f'<text x="{W/2:.0f}" y="26" text-anchor="middle" font-size="17" font-weight="700">{title}</text>',
             f'<defs><marker id="cs-{color[1:]}" markerWidth="7" markerHeight="7" refX="6.5" refY="3.5" '
             f'orient="auto"><polygon points="0,0 7,3.5 0,7" fill="{color}"/></marker></defs>']

    def cell(i):
        row = i // per_row
        cir = i % per_row
        col = cir if row % 2 == 0 else per_row - 1 - cir
        x = x0 + col * (bw + hgap)
        y = y0 + row * (bh + vgap)
        return x, y, row, col

    for i, (a, sub) in enumerate(stages):
        x, y, row, col = cell(i)
        parts.append(f'<rect x="{x}" y="{y}" width="{bw}" height="{bh}" rx="10" fill="{color}" '
                     f'opacity="0.13" stroke="{color}" stroke-width="2.5"/>')
        lines = _wrap_label(a)
        ty = y + (34 if len(lines) == 2 else 42)
        for j, ln in enumerate(lines):
            parts.append(f'<text x="{x+bw/2:.0f}" y="{ty+j*22:.0f}" text-anchor="middle" '
                         f'font-size="18" font-weight="700" fill="{INK}">{ln}</text>')
        if sub:
            parts.append(f'<text x="{x+bw/2:.0f}" y="{y+bh-14:.0f}" text-anchor="middle" '
                         f'font-size="13" fill="{MUT}">{sub}</text>')
        if i < n - 1:
            nx, ny, nrow, ncol = cell(i + 1)
            if nrow == row:  # horizontal arrow, direction of travel
                if ncol > col:
                    x1, x2 = x + bw, nx
                else:
                    x1, x2 = x, nx + bw
                yy = y + bh / 2
                parts.append(f'<line x1="{x1}" y1="{yy}" x2="{x2}" y2="{yy}" stroke="{color}" '
                             f'stroke-width="3" marker-end="url(#cs-{color[1:]})"/>')
            else:            # drop to next row (same visual column)
                cx = x + bw / 2
                parts.append(f'<line x1="{cx}" y1="{y+bh}" x2="{cx}" y2="{ny}" stroke="{color}" '
                             f'stroke-width="3" marker-end="url(#cs-{color[1:]})"/>')
    return f'<svg viewBox="0 0 {W} {H}" style="max-height:44vmin; width:100%;">\n  ' + "\n  ".join(parts) + "\n</svg>"


def vchain(title, stages, max_h=72):
    """Vertical block pipeline. stages = list of (label, sub, color, optional).
    `optional` draws a dashed outline + '(optionnel)' tag."""
    bw, bh, gap, x = 300, 46, 22, 80
    y0 = 44
    W = 460
    H = y0 + len(stages) * (bh + gap)
    parts = [f'<text x="{W/2:.0f}" y="26" text-anchor="middle" font-size="16" font-weight="700">{title}</text>',
             '<defs><marker id="vc-arr" markerWidth="6" markerHeight="6" refX="6" refY="3" orient="auto">'
             '<polygon points="0,0 6,3 0,6" fill="#1a1a1a"/></marker></defs>']
    for i, (lab, sub, col, opt) in enumerate(stages):
        y = y0 + i * (bh + gap)
        dash = ' stroke-dasharray="7 4"' if opt else ''
        parts.append(f'<rect x="{x}" y="{y}" width="{bw}" height="{bh}" rx="6" fill="{col}" '
                     f'opacity="0.14" stroke="{col}" stroke-width="2"{dash}/>')
        tag = '  (optionnel)' if opt else ''
        parts.append(f'<text x="{x+bw/2:.0f}" y="{y+21}" text-anchor="middle" font-size="14" '
                     f'font-weight="700" fill="{INK}">{lab}<tspan fill="{MUT}" font-size="11" font-weight="400">{tag}</tspan></text>')
        if sub:
            parts.append(f'<text x="{x+bw/2:.0f}" y="{y+38}" text-anchor="middle" font-size="11" fill="{MUT}">{sub}</text>')
        if i < len(stages) - 1:
            parts.append(f'<line x1="{x+bw/2:.0f}" y1="{y+bh}" x2="{x+bw/2:.0f}" y2="{y+bh+gap}" '
                         f'stroke="#1a1a1a" stroke-width="1.5" marker-end="url(#vc-arr)"/>')
    return f'<svg viewBox="0 0 {W} {H}" style="max-height:72vmin;">\n  ' + "\n  ".join(parts) + "\n</svg>"

TX_STAGES = [("Fichier", "image / data"), ("Compression", "+ RaptorQ"), ("Scrambler", "whitening"),
             ("LDPC", "FEC bits"), ("Mapping", "constellation"), ("RRC", "mise en forme"),
             ("Pilotes", "+ marqueurs"), ("Préambule", "+ VOX"), ("Audio 48k", "→ radio")]
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
      <li><strong>RaptorQ</strong> découpe le fichier en paquets et fabrique de la réparation&nbsp;; le <strong>scrambler</strong> blanchit ensuite les octets, juste <strong>avant le codage LDPC</strong> (spectre plat — détaillé plus loin)&nbsp;; <strong>LDPC</strong> blinde chaque bloc de bits.</li>
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
        <li>Donc en <strong>NBFM</strong>&nbsp;: pas besoin de NCO. Un simple <strong>bloc anti-DC</strong> (passe-haut) suffit&nbsp;; l'égaliseur + la DD-PLL font le reste.</li>
        <li>Mais pour <strong>QO-100 / SSB</strong> (porteuse supprimée, transpondeur linéaire), l'écart porteuse redevient réel&nbsp;: on le mesure et on le corrige — c'est le bloc <strong>CFO</strong> (quelques slides plus loin).</li>
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

# ===========================================================================
# PART 1 — a word on SDR + QO-100 (accessible level)
# ===========================================================================
P_SDR = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · Au-delà du micro</div>
  <h2>Récepteurs SDR et ouverture sur QO-100</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>Au départ, le modem écoute le <strong>BF</strong> via une <strong>carte son</strong>, une interface type <strong>SignaLink</strong>, ou la <strong>carte son interne de la radio</strong> (FT-991A, FTX-1, …). Simple, mais on hérite du bruit et des filtres du poste.</li>
        <li>Il sait aussi lire directement une <strong>clé / récepteur SDR</strong> (radio définie par logiciel)&nbsp;: <strong>RTL-SDR</strong>, <strong>SDRplay</strong>, <strong>PlutoSDR</strong> → le signal arrive avec une cascade et un S-mètre.</li>
        <li>Un onglet <strong>Radio</strong> intégré&nbsp;: accord, spectre, cascade — on voit la station avant de décoder.</li>
        <li>Et surtout&nbsp;: ça ouvre le <strong>satellite <span class="mono">QO-100</span></strong> (Es'hail-2), accessible en <strong>SSB</strong>. Le canal n'est pas «&nbsp;plus propre&nbsp;» mais <strong>linéaire</strong> (pas de limiteur), avec un <strong>bruit gaussien qui domine</strong> — un régime bien plus favorable aux constellations denses.</li>
        <li>Le PlutoSDR fait <strong>émission + réception</strong> sur QO-100&nbsp;: on s'entend soi-même sur le satellite. La partie technique arrive «&nbsp;sous le capot&nbsp;».</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 420 300" style="max-height:56vmin;">
        <rect x="0" y="0" width="420" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="210" y="26" text-anchor="middle" font-size="14" font-weight="700">Trois manières d'écouter</text>
        <rect x="30" y="60" width="150" height="46" rx="8" fill="{SOFT}"/>
        <text x="105" y="88" text-anchor="middle" font-size="13" font-weight="700">Carte son</text>
        <rect x="30" y="120" width="150" height="46" rx="8" fill="{ACC2}" opacity="0.15" stroke="{ACC2}" stroke-width="2"/>
        <text x="105" y="148" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">SDR (RTL / RSP)</text>
        <rect x="30" y="180" width="150" height="46" rx="8" fill="{OK}" opacity="0.15" stroke="{OK}" stroke-width="2"/>
        <text x="105" y="208" text-anchor="middle" font-size="13" font-weight="700" fill="{OK}">Pluto (TX+RX)</text>
        <text x="105" y="250" text-anchor="middle" font-size="11" fill="{MUT}">→ modem</text>
        <circle cx="330" cy="90" r="20" fill="{ACC}" opacity="0.18" stroke="{ACC}" stroke-width="2"/>
        <text x="330" y="94" text-anchor="middle" font-size="16">🛰️</text>
        <text x="330" y="130" text-anchor="middle" font-size="12" font-weight="700" fill="{ACC}">QO-100</text>
        <text x="330" y="148" text-anchor="middle" font-size="10" fill="{MUT}">géostationnaire</text>
        <path d="M 250 205 C 300 205 300 120 322 108" fill="none" stroke="{OK}" stroke-width="2" stroke-dasharray="4 3"/>
        <path d="M 322 108 C 300 150 300 200 250 208" fill="none" stroke="{ACC2}" stroke-width="2" stroke-dasharray="4 3"/>
        <text x="300" y="250" text-anchor="middle" font-size="10" fill="{MUT}">montée / descente</text>
      </svg>
    </div>
  </div>
</section>
"""

# PART 1 — accessible teaser for the turbo receiver (after SDR / QO-100)
P_TURBO_TEASER = f"""
<section class="slide">
  <div class="eyebrow">Partie 1 · La nouveauté</div>
  <h2>Le récepteur «&nbsp;turbo&nbsp;», en une image</h2>
  <div class="two-col two-col-2-1">
    <div class="col">
      <ul>
        <li>Un récepteur radio doit <strong>redresser</strong> le signal (canal, échos) <em>et</em> <strong>corriger les erreurs</strong> (décodage).</li>
        <li>Avant&nbsp;: on faisait les deux <strong>l'un après l'autre</strong>, une seule fois.</li>
        <li>Maintenant&nbsp;: les deux <strong>se parlent en boucle</strong> — le décodeur dit au «&nbsp;redresseur&nbsp;» ce qu'il a compris, qui se corrige, ce qui aide encore le décodeur… <strong>c'est le principe «&nbsp;turbo&nbsp;»</strong>.</li>
        <li>Résultat&nbsp;: on décode <strong>plus bas dans le bruit</strong> — précieux sur QO-100 et les relais lointains.</li>
        <li>Le «&nbsp;sous le capot&nbsp;» détaille comment (recalage fréquence, horloge, blanchiment).</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 260 240" style="max-height:46vmin;">
        <rect x="0" y="0" width="260" height="240" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <rect x="55" y="35" width="150" height="52" rx="10" fill="{ACC2}" opacity="0.15" stroke="{ACC2}" stroke-width="2"/>
        <text x="130" y="60" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">Redresser</text>
        <text x="130" y="78" text-anchor="middle" font-size="10" fill="{MUT}">(égaliseur)</text>
        <rect x="55" y="150" width="150" height="52" rx="10" fill="{OK}" opacity="0.15" stroke="{OK}" stroke-width="2"/>
        <text x="130" y="175" text-anchor="middle" font-size="13" font-weight="700" fill="{OK}">Corriger</text>
        <text x="130" y="193" text-anchor="middle" font-size="10" fill="{MUT}">(décodeur)</text>
        <path d="M 120 87 L 120 148" stroke="{ACC2}" stroke-width="2.4" marker-end="url(#tt-d)"/>
        <path d="M 140 150 L 140 89" stroke="{ACC}" stroke-width="2.4" stroke-dasharray="6 4" marker-end="url(#tt-u)"/>
        <defs>
          <marker id="tt-d" markerWidth="8" markerHeight="8" refX="6" refY="4" orient="auto"><polygon points="0,0 8,4 0,8" fill="{ACC2}"/></marker>
          <marker id="tt-u" markerWidth="8" markerHeight="8" refX="6" refY="4" orient="auto"><polygon points="0,0 8,4 0,8" fill="{ACC}"/></marker>
        </defs>
        <text x="205" y="120" text-anchor="middle" font-size="11" fill="{ACC}" font-weight="700">boucle</text>
      </svg>
    </div>
  </div>
</section>
"""

# ===========================================================================
# PART 2 — the turbo receiver (CFO / SFO / turbo loop / scrambler)
# ===========================================================================
TURBO_DIVIDER = """
<section class="slide section">
  <div class="ch-num">PARTIE 2 · LE RÉCEPTEUR TURBO</div>
  <h1>Le récepteur turbo</h1>
  <div class="sub">Trois recalages emboîtés — fréquence porteuse (CFO), horloge (SFO), et la boucle turbo canal↔bits — plus un blanchiment à l'émission.</div>
</section>
"""

SOFTBIT_SLIDE = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Prérequis</div>
  <h2>Bit «&nbsp;dur&nbsp;» vs bit «&nbsp;souple&nbsp;»&nbsp;: parler en probabilités</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>Un <strong>bit dur</strong>, c'est une décision sèche&nbsp;: «&nbsp;c'est un 0&nbsp;» ou «&nbsp;c'est un 1&nbsp;». On jette toute l'information sur la <em>confiance</em>.</li>
        <li>Un <strong>bit souple</strong> garde la <strong>probabilité</strong>&nbsp;: «&nbsp;c'est un 0 à 80&nbsp;%&nbsp;». On l'exprime par un <strong>LLR</strong> (log-rapport de vraisemblance)&nbsp;: signe = 0 ou 1, amplitude = certitude.</li>
        <li>Près d'un point de constellation → LLR grand (sûr). À mi-chemin entre deux → LLR ≈ 0 (on ne sait pas).</li>
        <li>Le décodeur <strong>LDPC souple</strong> exploite ces nuances&nbsp;: un bit «&nbsp;à 55&nbsp;%&nbsp;» n'entraîne pas d'erreur ferme, il est simplement <em>pondéré</em>.</li>
        <li><strong>C'est ce qui rend le turbo possible</strong>&nbsp;: on peut réinjecter une «&nbsp;probabilité&nbsp;» dans l'égaliseur, pas une décision fausse.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 380 300" style="max-height:54vmin;">
        <rect x="0" y="0" width="380" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="190" y="26" text-anchor="middle" font-size="13" font-weight="700">Reçu entre deux symboles</text>
        <line x1="40" y1="150" x2="340" y2="150" stroke="{SOFT}"/>
        <line x1="190" y1="60" x2="190" y2="240" stroke="{SOFT}"/>
        <circle cx="110" cy="150" r="9" fill="{ACC2}"/>
        <text x="110" y="178" text-anchor="middle" font-size="12" font-weight="700" fill="{ACC2}">0</text>
        <circle cx="270" cy="150" r="9" fill="{OK}"/>
        <text x="270" y="178" text-anchor="middle" font-size="12" font-weight="700" fill="{OK}">1</text>
        <circle cx="205" cy="150" r="7" fill="{ACC}"/>
        <text x="205" y="132" text-anchor="middle" font-size="11" fill="{ACC}">reçu</text>
        <text x="190" y="215" text-anchor="middle" font-size="12" fill="{MUT}">plus près du «&nbsp;1&nbsp;» → LLR &gt; 0, mais faible</text>
        <rect x="60" y="245" width="260" height="26" rx="6" fill="{SOFT}"/>
        <rect x="60" y="245" width="150" height="26" rx="6" fill="{OK}" opacity="0.5"/>
        <text x="190" y="263" text-anchor="middle" font-size="11" font-weight="700">≈ 58 % que ce soit un 1</text>
      </svg>
    </div>
  </div>
</section>
"""

TURBO_CORE = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Le cœur turbo</div>
  <h2>La boucle turbo&nbsp;: le canal aide les bits, les bits aident le canal</h2>
  <div class="two-col two-col-2-1">
    <div class="col">
      <ul>
        <li>Chaîne classique&nbsp;: on égalise <strong>une fois</strong>, on décode <strong>une fois</strong>. Le décodeur ne renvoie jamais ce qu'il a appris.</li>
        <li>Turbo (Tüchler / Koetter / Singer, 2002)&nbsp;: jusqu'à <strong>5 passes</strong> par segment. À chaque tour, le <strong>LDPC souple</strong> renvoie des symboles «&nbsp;probables&nbsp;» qui ré-entraînent l'égaliseur et la phase.</li>
        <li>Une passe = <strong>FFE + DD-PLL + démapping souple + LDPC + ré-injection</strong>. On <strong>s'arrête dès que tous les codewords sont bons</strong> (syndrome nul).</li>
        <li>Chaque donnée est pondérée par sa <strong>fiabilité</strong> <span class="mono">w = |E[a]|²/E[|a|²]</span> (nulle au 1ᵉʳ tour, croît quand on devient sûr).</li>
        <li><strong>Tout est souple, jamais de décision dure</strong>&nbsp;: un segment massacré donne un poids ≈ 0 → il ne peut pas <strong>empoisonner</strong> l'égaliseur. Robuste à bas SNR.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 300 320" style="max-height:58vmin;">
        <rect x="0" y="0" width="300" height="320" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="150" y="26" text-anchor="middle" font-size="13" font-weight="700">×5 passes max</text>
        <rect x="80" y="45"  width="140" height="42" rx="8" fill="{ACC2}" opacity="0.14" stroke="{ACC2}" stroke-width="2"/>
        <text x="150" y="71" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">FFE (égaliseur)</text>
        <rect x="80" y="107" width="140" height="42" rx="8" fill="{ACC2}" opacity="0.14" stroke="{ACC2}" stroke-width="2"/>
        <text x="150" y="133" text-anchor="middle" font-size="13" font-weight="700" fill="{ACC2}">DD-PLL (phase)</text>
        <rect x="80" y="169" width="140" height="42" rx="8" fill="{ACC2}" opacity="0.14" stroke="{ACC2}" stroke-width="2"/>
        <text x="150" y="188" text-anchor="middle" font-size="12" font-weight="700" fill="{ACC2}">Démap souple</text>
        <text x="150" y="203" text-anchor="middle" font-size="10" fill="{MUT}">→ LLR</text>
        <rect x="80" y="231" width="140" height="42" rx="8" fill="{OK}" opacity="0.14" stroke="{OK}" stroke-width="2"/>
        <text x="150" y="257" text-anchor="middle" font-size="13" font-weight="700" fill="{OK}">LDPC SISO</text>
        <g stroke="{ACC2}" stroke-width="2" fill="none">
          <line x1="150" y1="87"  x2="150" y2="105"/>
          <line x1="150" y1="149" x2="150" y2="167"/>
          <line x1="150" y1="211" x2="150" y2="229"/>
        </g>
        <path d="M 80 252 C 30 252 30 66 78 66" fill="none" stroke="{ACC}" stroke-width="2.4" stroke-dasharray="6 4"/>
        <polygon points="78,66 86,61 86,71" fill="{ACC}"/>
        <text x="20" y="160" font-size="11" fill="{ACC}" font-weight="700" transform="rotate(-90 20 160)">symboles souples (ré-injection)</text>
        <text x="150" y="295" text-anchor="middle" font-size="11" fill="{MUT}">arrêt sur syndrome nul</text>
      </svg>
    </div>
  </div>
</section>
"""

CFO_SLIDE = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Recalage fréquence (CFO)</div>
  <h2>CFO&nbsp;: recentrer la porteuse en SSB / QO-100</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>En <strong>SSB / QO-100</strong> (porteuse supprimée), les oscillateurs TX et RX diffèrent → le signal arrive décalé d'un <strong>offset constant</strong> (~108 Hz mesuré sur l'air).</li>
        <li>Le filtre adapté du préambule a un <strong>lobe principal de ±6 Hz</strong>&nbsp;: même 100 Hz de décalage <strong>détruit la détection</strong>. Il faut donc le mesurer.</li>
        <li>Estimation en <strong>deux étages, une seule fois par salve</strong>, sur ±250 Hz&nbsp;:</li>
        <li><span class="chip">Grossier</span> périodogramme (FFT 16384&nbsp;→&nbsp;2,93&nbsp;Hz/bin) corrélé à un <strong>gabarit de bande RRC</strong> → robuste à un QRM étroit (il intègre toute la bande).</li>
        <li><span class="chip">Fin</span> grille ±15 Hz notée par la <strong>métrique du filtre adapté</strong> + interpolation parabolique → la valeur réellement appliquée.</li>
        <li><strong>Zone morte&nbsp;: |CFO| &lt; 1,5 Hz → 0 exact</strong> → sur signal propre, chemin bit-à-bit identique (aucune régression). L'offset pilote un NCO de descente, verrouillé une fois par salve.</li>
      </ul>
    </div>
    <div class="col">
      <img src="presentation_assets/hb9mm_cfo_anim.gif" alt="spectre du préambule décalé et gabarit glissant qui verrouille le CFO" style="width:100%; max-height:58vmin; object-fit:contain; border:2px solid {SOFT}; border-radius:6px;">
      <div class="caption" style="margin-top:0.6vmin;">Le préambule reçu (rouge) est décalé de +108 Hz&nbsp;; le gabarit (bleu) glisse jusqu'au recouvrement → verrouillage.</div>
    </div>
  </div>
</section>
"""

SFO_SLIDE = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Recalage horloge (SFO)</div>
  <h2>SFO&nbsp;: rattraper la dérive des horloges d'échantillonnage</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>Les horloges des <strong>convertisseurs A/D (RX) et D/A (TX)</strong> — les quartz des cartes son / SDR — dérivent l'une par rapport à l'autre, <strong>quelques dizaines de ppm</strong> → le rythme des symboles glisse.</li>
        <li><strong>Timing assisté par les données</strong> sur le préambule connu (module constant)&nbsp;: interpolation parabolique de la magnitude du filtre adapté → phase de timing τ.</li>
        <li>Deux préambules <strong>espacés de ~4 s</strong> → la <strong>pente de τ</strong> donne la dérive résiduelle, intégrée (gain 0,5) dans le débit du <strong>ré-échantillonneur lisse</strong>.</li>
        <li>Appliqué par un <strong>accumulateur de phase continu</strong> (changement de vitesse doux, sans à-coup) → boucle <strong>type 2</strong>&nbsp;: erreur permanente nulle face à une dérive.</li>
        <li>Le détecteur est <strong>avant le FFE</strong> et le ré-échantillonneur est le <strong>seul actionneur</strong> de timing, séparé de la boucle turbo → pas de boucles qui se battent. Mesuré&nbsp;: <strong>+50 ppm sans pénalité</strong>.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 420 300" style="max-height:56vmin;">
        <rect x="0" y="0" width="420" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="210" y="26" text-anchor="middle" font-size="13" font-weight="700">Deux préambules, ~4 s d'écart</text>
        <line x1="30" y1="140" x2="400" y2="140" stroke="{INK}"/>
        <path d="M 60 140 C 78 90 92 90 110 140" fill="none" stroke="{ACC2}" stroke-width="2.4"/>
        <text x="85" y="165" text-anchor="middle" font-size="11" fill="{MUT}">τ₁</text>
        <path d="M 300 140 C 320 90 334 90 352 140" fill="none" stroke="{ACC2}" stroke-width="2.4"/>
        <text x="330" y="165" text-anchor="middle" font-size="11" fill="{MUT}">τ₂</text>
        <line x1="90" y1="80" x2="330" y2="98" stroke="{ACC}" stroke-width="2" stroke-dasharray="5 3"/>
        <text x="210" y="78" text-anchor="middle" font-size="12" fill="{ACC}" font-weight="700">pente = dérive (SFO)</text>
        <text x="210" y="185" text-anchor="middle" font-size="11" fill="{MUT}">t → (≈ 4 s entre les deux)</text>
        <rect x="70" y="215" width="280" height="60" rx="8" fill="{OK}" opacity="0.12" stroke="{OK}" stroke-width="2"/>
        <text x="210" y="240" text-anchor="middle" font-size="12" font-weight="700" fill="{OK}">ré-échantillonneur lisse</text>
        <text x="210" y="260" text-anchor="middle" font-size="10" fill="{MUT}">débit corrigé en douceur — boucle type 2</text>
      </svg>
    </div>
  </div>
</section>
"""

SCRAMBLER_SLIDE = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Blanchiment (scrambler)</div>
  <h2>Scrambler&nbsp;: blanchir le signal pour tenir sur QO-100</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>Un bloc à <strong>faible entropie</strong> (zéros, méta répétitive) a une moyenne biaisée → le modulateur émet une <strong>raie / porteuse résiduelle</strong> à 1100 Hz qui <strong>plafonne la puissance crête</strong> et gaspille la linéarité de QO-100.</li>
        <li>Remède&nbsp;: <strong>dispersion d'énergie</strong> des octets utiles → 0 et 1 équilibrés (≈ 0,5) → <strong>spectre plat, PAPR normalisée, plus de raie</strong>.</li>
        <li>Scrambler <strong>G3RUH auto-synchronisant</strong>, polynôme <span class="mono">1 + x¹² + x¹⁷</span> — un simple XOR pseudo-aléatoire réversible, qui ne touche <strong>ni la modulation ni le codage</strong>.</li>
        <li>Appliqué <strong>hors de l'enveloppe FEC</strong> (avant le codage à l'émission, après le décodage à la réception) → LDPC, Golay, entrelaceur et boucle turbo restent <strong>intacts</strong>.</li>
        <li><strong>Jamais</strong> blanchis&nbsp;: préambule, sync du marker, warmup, pilotes — ce sont des références de corrélation, déjà à moyenne nulle.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 420 300" style="max-height:56vmin;">
        <rect x="0" y="0" width="420" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="115" y="26" text-anchor="middle" font-size="12" font-weight="700" fill="{ACC}">Sans&nbsp;: une raie</text>
        <line x1="30" y1="130" x2="200" y2="130" stroke="{INK}"/>
        <path d="M 40 130 C 70 122 160 122 190 130" fill="none" stroke="{MUT}" stroke-width="1.2"/>
        <line x1="115" y1="130" x2="115" y2="45" stroke="{ACC}" stroke-width="3"/>
        <polygon points="115,45 110,58 120,58" fill="{ACC}"/>
        <text x="305" y="26" text-anchor="middle" font-size="12" font-weight="700" fill="{OK}">Après&nbsp;: plat</text>
        <line x1="220" y1="130" x2="390" y2="130" stroke="{INK}"/>
        <path d="M 230 118 L 245 120 L 258 116 L 272 121 L 286 117 L 300 120 L 314 116 L 328 121 L 342 117 L 356 119 L 380 118" fill="none" stroke="{OK}" stroke-width="1.6"/>
        <text x="210" y="185" text-anchor="middle" font-size="12" font-weight="700">Registre G3RUH (17 bits)</text>
        {''.join(f'<rect x="{60+i*18}" y="205" width="16" height="26" fill="{"#fff"}" stroke="{SOFT}"/>' for i in range(17))}
        <line x1="{60+11*18+8}" y1="205" x2="{60+11*18+8}" y2="255" stroke="{ACC}" stroke-width="1.5"/>
        <text x="{60+11*18+8}" y="270" text-anchor="middle" font-size="10" fill="{ACC}">x¹²</text>
        <line x1="{60+16*18+8}" y1="205" x2="{60+16*18+8}" y2="255" stroke="{ACC}" stroke-width="1.5"/>
        <text x="{60+16*18+8}" y="270" text-anchor="middle" font-size="10" fill="{ACC}">x¹⁷</text>
        <text x="210" y="292" text-anchor="middle" font-size="10" fill="{MUT}">taps additionnés (XOR) → sortie blanchie</text>
      </svg>
    </div>
  </div>
</section>
"""

TURBO_RECAP = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Récapitulatif</div>
  <h2>Qui recale quoi, et à quelle cadence</h2>
  <div class="body" style="margin-bottom:1.5vmin;">Quatre traitements <strong>orthogonaux</strong> — chacun possède sa propre grandeur, donc ils ne se battent pas&nbsp;:</div>
  <table class="profiles">
    <tr><th>Bloc</th><th>Grandeur recalée</th><th>Méthode</th><th>Cadence</th></tr>
    <tr><td><strong>CFO</strong></td><td>Fréquence porteuse (SSB / QO-100)</td><td>Welch + gabarit RRC, puis grille filtre adapté ±15 Hz</td><td>1× par salve</td></tr>
    <tr><td><strong>SFO</strong></td><td>Horloge d'échantillonnage (A/D, D/A)</td><td>Timing 2-préambules → ré-échantillonneur, boucle type 2</td><td>Continu</td></tr>
    <tr class="hi"><td><strong>Boucle turbo</strong></td><td>Canal + phase + bits</td><td>FFE + DD-PLL souples ↔ LDPC SISO, ≤ 5 passes</td><td>Par segment</td></tr>
    <tr><td><strong>Scrambler</strong></td><td>Statistique du signal (PAPR / raie)</td><td>Whitening G3RUH, hors enveloppe FEC</td><td>Chaque octet (TX)</td></tr>
  </table>
  <div class="caption">C'est ce découplage qui rend le récepteur turbo robuste à bas SNR sur QO-100.</div>
</section>
"""

FLYWHEEL_SLIDE = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Entrée tardive</div>
  <h2>Backward flywheel&nbsp;: récupérer ce qui est arrivé avant le lock</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>Un RX qui démarre <strong>en cours de salve</strong> (<em>late entry</em>) rate le début. Il se verrouille sur le <strong>premier marker</strong> qu'il croise&nbsp;: c'est l'<strong>ancre</strong>.</li>
        <li><strong>Rejeu en arrière</strong>&nbsp;: depuis l'ancre, on <strong>rembobine l'état DSP</strong> (timing + canal déjà établis) et on rejoue le buffer <strong>à l'envers</strong> pour décoder les codewords arrivés <strong>avant</strong> le lock → on récupère le <strong>début de l'image</strong>.</li>
        <li><strong>Coast en avant</strong>&nbsp;: à travers un marker manqué (fade), on continue en aveugle. On n'accepte que les codewords qui <strong>convergent</strong> (auto-validés)&nbsp;: le bruit ne converge jamais → il ne peut pas empoisonner.</li>
        <li>Rejeu <strong>déterministe, byte-exact</strong>&nbsp;: rembobinage sans copie jusqu'à une marge de warmup, puis replay jusqu'à la tête (reproduit le canal que le FFE n'avait jamais vu).</li>
        <li>Coût <strong>nul</strong> sur un démarrage propre&nbsp;; activé par défaut.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 470 300" style="max-height:54vmin;">
        <rect x="0" y="0" width="470" height="300" fill="#fff" stroke="{SOFT}" stroke-width="2"/>
        <text x="235" y="30" text-anchor="middle" font-size="14" font-weight="700">Salve en cours — le RX arrive au milieu</text>
        <!-- buffered (pre-entry) region -->
        <rect x="20" y="118" width="206" height="52" fill="{ACC}" opacity="0.06"/>
        <text x="120" y="110" text-anchor="middle" font-size="11" fill="{MUT}">déjà passé (en buffer)</text>
        <!-- blocks: C C M C M(anchor) C C M -->
        {''.join(f'<rect x="{20+i*52}" y="118" width="48" height="52" rx="5" fill="{ACC2 if i in (1,4,7) else OK}" opacity="{0.9 if i!=4 else 1}" stroke="{ACC if i==4 else "none"}" stroke-width="{3 if i==4 else 0}"/><text x="{20+i*52+24}" y="149" text-anchor="middle" fill="#fff" font-size="13" font-weight="700">{"M" if i in (1,4,7) else "CW"}</text>' for i in range(8))}
        <!-- entry line -->
        <line x1="226" y1="92" x2="226" y2="182" stroke="{INK}" stroke-width="2" stroke-dasharray="4 3"/>
        <text x="226" y="86" text-anchor="middle" font-size="12" font-weight="700" fill="{INK}">RX démarre</text>
        <text x="252" y="205" text-anchor="middle" font-size="11" fill="{ACC}" font-weight="700">ancre</text>
        <!-- backward arrow -->
        <line x1="224" y1="235" x2="30" y2="235" stroke="{ACC}" stroke-width="3" marker-end="url(#fb-l)"/>
        <defs>
          <marker id="fb-l" markerWidth="9" markerHeight="9" refX="7" refY="4.5" orient="auto"><polygon points="9,0 0,4.5 9,9" fill="{ACC}"/></marker>
          <marker id="fb-r" markerWidth="9" markerHeight="9" refX="7" refY="4.5" orient="auto"><polygon points="0,0 9,4.5 0,9" fill="{OK}"/></marker>
        </defs>
        <text x="125" y="258" text-anchor="middle" font-size="12" fill="{ACC}" font-weight="700">flyback&nbsp;: rejeu en arrière</text>
        <!-- forward arrow -->
        <line x1="240" y1="235" x2="446" y2="235" stroke="{OK}" stroke-width="3" marker-end="url(#fb-r)"/>
        <text x="350" y="258" text-anchor="middle" font-size="12" fill="{OK}" font-weight="700">décodage avant (normal)</text>
      </svg>
    </div>
  </div>
</section>
"""

# Complete RX block diagram: acquisition/tracking loops (CFO/SFO/estimator)
# feeding the datapath, then the turbo stage with its feedback loop.
def _bd_box(cx, y, w, h, label, sub, fill, stroke, fs=17):
    x = cx - w / 2
    lines = label.split("\n")
    ty = y + (h/2 - 8*(len(lines)-1)) + 6
    out = [f'<rect x="{x:.0f}" y="{y}" width="{w}" height="{h}" rx="9" fill="{fill}" stroke="{stroke}" stroke-width="2.5"/>']
    for j, ln in enumerate(lines):
        out.append(f'<text x="{cx:.0f}" y="{ty+j*19:.0f}" text-anchor="middle" font-size="{fs}" font-weight="700" fill="{INK}">{ln}</text>')
    if sub:
        out.append(f'<text x="{cx:.0f}" y="{y+h-9:.0f}" text-anchor="middle" font-size="12" fill="{MUT}">{sub}</text>')
    return "\n  ".join(out)

_BOX = "#f3f1ea"
_dp = [(110, "Entrée\nBF / SDR", ""), (300, "NCO\ndownmix", ""), (490, "Ré-échant.", "resampler"),
       (680, "Filtre\nadapté RRC", ""), (870, "FFE", "égaliseur"), (1060, "Corr.\nporteuse φ", "")]
_dp_parts = []
for cx, lab, sub in _dp:
    _dp_parts.append(_bd_box(cx, 250, 150, 78, lab, sub, _BOX, MUT))
for i in range(len(_dp) - 1):
    _dp_parts.append(f'<line x1="{_dp[i][0]+75}" y1="289" x2="{_dp[i+1][0]-75}" y2="289" stroke="{INK}" stroke-width="2.5" marker-end="url(#bd-main)"/>')

TURBO_BLOCKDIAG = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · Vue d'ensemble</div>
  <h2>Le récepteur turbo, schéma bloc complet</h2>
  <div class="full-image">
    <svg viewBox="0 0 1320 560" style="max-height:66vmin; width:100%;">
      <defs>
        <marker id="bd-main" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><polygon points="0,0 8,4 0,8" fill="{INK}"/></marker>
        <marker id="bd-cfo" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><polygon points="0,0 8,4 0,8" fill="{ACC}"/></marker>
        <marker id="bd-sfo" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><polygon points="0,0 8,4 0,8" fill="{OK}"/></marker>
        <marker id="bd-est" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><polygon points="0,0 8,4 0,8" fill="{WARN}"/></marker>
        <marker id="bd-turbo" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><polygon points="0,0 8,4 0,8" fill="{ACC2}"/></marker>
      </defs>

      <!-- estimators feeding the datapath (the correction loops before turbo) -->
      {_bd_box(300, 60, 180, 70, "CFO — acquisition", "préambule · 2 étages", "#fff", ACC, 15)}
      {_bd_box(490, 60, 180, 70, "SFO — 2 préambules", "dérive d'horloge", "#fff", OK, 15)}
      {_bd_box(1060, 60, 190, 70, "Estimateur porteuse", "RxEstimator", "#fff", WARN, 15)}
      <line x1="300" y1="130" x2="300" y2="248" stroke="{ACC}" stroke-width="2.5" marker-end="url(#bd-cfo)"/>
      <text x="314" y="205" font-size="12" fill="{ACC}" font-weight="700">1×/salve</text>
      <line x1="490" y1="130" x2="490" y2="248" stroke="{OK}" stroke-width="2.5" marker-end="url(#bd-sfo)"/>
      <text x="504" y="205" font-size="12" fill="{OK}" font-weight="700">continu</text>
      <line x1="1060" y1="130" x2="1060" y2="248" stroke="{WARN}" stroke-width="2.5" marker-end="url(#bd-est)"/>

      <!-- datapath -->
      {''.join(_dp_parts)}

      <!-- into the turbo stage -->
      <line x1="1060" y1="328" x2="1060" y2="410" stroke="{INK}" stroke-width="2.5" marker-end="url(#bd-main)"/>
      {_bd_box(840, 412, 560, 82, "ÉTAGE TURBO — démap souple ↔ LDPC SISO", "boucle ≤ 5 passes · arrêt sur syndrome nul", ACC2 + "22", ACC2, 17)}
      {_bd_box(1215, 412, 150, 82, "→ bits", "RaptorQ", _BOX, MUT, 16)}
      <line x1="1120" y1="453" x2="1140" y2="453" stroke="{INK}" stroke-width="2.5" marker-end="url(#bd-main)"/>

      <!-- turbo feedback loop to FFE + phase -->
      <path d="M 700 412 C 620 360, 790 300, 856 330" fill="none" stroke="{ACC2}" stroke-width="2.6" stroke-dasharray="7 4" marker-end="url(#bd-turbo)"/>
      <text x="560" y="400" font-size="13" fill="{ACC2}" font-weight="700">symboles souples → ré-entraîne FFE + φ</text>

      <!-- legend -->
      <g font-size="13">
        <line x1="30" y1="524" x2="70" y2="524" stroke="{ACC}" stroke-width="3"/><text x="76" y="528" fill="{INK}">CFO (fréquence porteuse)</text>
        <line x1="300" y1="524" x2="340" y2="524" stroke="{OK}" stroke-width="3"/><text x="346" y="528" fill="{INK}">SFO (horloge)</text>
        <line x1="470" y1="524" x2="510" y2="524" stroke="{WARN}" stroke-width="3"/><text x="516" y="528" fill="{INK}">phase</text>
        <line x1="600" y1="524" x2="640" y2="524" stroke="{ACC2}" stroke-width="3" stroke-dasharray="7 4"/><text x="646" y="528" fill="{INK}">boucle turbo (≤ 5 passes)</text>
      </g>
    </svg>
  </div>
  <div class="caption">Avant l'étage turbo&nbsp;: le CFO cale la fréquence (1×/salve), le SFO pilote le ré-échantillonneur (continu), l'estimateur suit la phase. Puis la boucle turbo affine FFE + phase avec les symboles souples du LDPC.</div>
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
    <div class="toc-item"><span class="toc-num">P1</span><span class="toc-title">SDR &amp; ouverture QO-100</span></div>
    <div class="toc-item"><span class="toc-num">P2</span><span class="toc-title">Récepteur turbo&nbsp;: CFO / SFO / boucle / scrambler</span></div>
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
    "HB9MM · vendredi 10 juillet 2026")
title_slide = title_slide.replace(
    "transmettre des images sur la FM amateur",
    "transmettre des images sur la FM amateur… et plus")
title_slide = title_slide.replace(
    "Du concept OFDM abandonné jusqu'au modem V3 multi-profil",
    "Des bases de la radio numérique au récepteur turbo — FM, SDR, QO-100")

# early "objectif" slide — state both channel-spacing deviations
b[2] = b[2].replace(
    "déviation ±2,5 kHz",
    "déviation ±2,5 kHz (canal 12,5 kHz) ou ±5 kHz (canal 25 kHz)")

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
for k in (4, 7, 12, 16, 26, 30, 51, 55, 59):   # annex dividers
    b[k] = relabel(b[k], "ANNEXE · POUR LES CURIEUX")

# ---------------------------------------------------------------------------
# V4 frame edits on reused blocks
# ---------------------------------------------------------------------------
MK = "#4d83b3"; WU = "#d96b4e"

# frame chapter divider — present tense only (the frame IS this; no history)
b[33] = """
<section class="slide section">
  <div class="ch-num">PARTIE 2 · SOUS LE CAPOT</div>
  <h1>La structure de trame</h1>
  <div class="sub">Une trame à segments cycliques&nbsp;: chaque brique existe pour une raison précise côté récepteur.</div>
</section>
"""

# slide 24 — the current V4 frame, described in the present (no evolution narrative)
b[35] = f"""
<section class="slide">
  <div class="eyebrow">Partie 2 · La trame</div>
  <h2>La trame&nbsp;: 4 codewords et 2 blocs markers par période</h2>
  <div class="full-image">
    <svg viewBox="0 0 1180 300" style="max-height:56vmin; width:100%;">
      <!-- period bracket -->
      <g stroke="{ACC}" stroke-width="2" fill="none">
        <line x1="20" y1="70" x2="1120" y2="70"/><line x1="20" y1="64" x2="20" y2="76"/><line x1="1120" y1="64" x2="1120" y2="76"/>
      </g>
      <text x="570" y="56" text-anchor="middle" font-size="16" fill="{ACC}" font-weight="700">Période ~4 s ≈ 4 codewords (profils rapides) — préambule + marker rejoués</text>

      <!-- preamble -->
      <rect x="20" y="100" width="150" height="86" rx="6" fill="{ACC}"/>
      <text x="95" y="140" text-anchor="middle" fill="#fff" font-size="16" font-weight="700">Préambule</text>
      <text x="95" y="162" text-anchor="middle" fill="#f3d4ca" font-size="13">256 QPSK</text>

      <!-- marker 1 (bootstrap) -->
      <rect x="185" y="100" width="175" height="86" rx="6" fill="{MK}" stroke="{ACC}" stroke-width="3"/>
      <text x="272" y="136" text-anchor="middle" fill="#fff" font-size="16" font-weight="700">Marker 1</text>
      <text x="272" y="157" text-anchor="middle" fill="#fff" font-size="12">profil + flags</text>
      <text x="272" y="175" text-anchor="middle" fill="#dbe8f2" font-size="12">128 sym · QPSK</text>

      <!-- warmup -->
      <rect x="375" y="100" width="105" height="86" rx="6" fill="{WU}"/>
      <text x="427" y="140" text-anchor="middle" fill="#fff" font-size="15" font-weight="700">Warmup</text>
      <text x="427" y="162" text-anchor="middle" fill="#fbe4dc" font-size="12">32 sym</text>

      <!-- segment 1 : 2 CW -->
      <rect x="495" y="100" width="105" height="86" rx="6" fill="{OK}" opacity="0.85"/>
      <text x="547" y="140" text-anchor="middle" fill="#fff" font-size="16" font-weight="700">CW 1</text>
      <rect x="610" y="100" width="105" height="86" rx="6" fill="{OK}" opacity="0.85"/>
      <text x="662" y="140" text-anchor="middle" fill="#fff" font-size="16" font-weight="700">CW 2</text>
      <text x="605" y="205" text-anchor="middle" font-size="13" fill="{MUT}">segment 1 · +&nbsp;pilotes</text>

      <!-- marker 2 -->
      <rect x="730" y="100" width="150" height="86" rx="6" fill="{MK}"/>
      <text x="805" y="140" text-anchor="middle" fill="#fff" font-size="16" font-weight="700">Marker 2</text>
      <text x="805" y="162" text-anchor="middle" fill="#dbe8f2" font-size="12">128 sym · QPSK</text>

      <!-- segment 2 : 2 CW -->
      <rect x="895" y="100" width="105" height="86" rx="6" fill="{OK}" opacity="0.85"/>
      <text x="947" y="140" text-anchor="middle" fill="#fff" font-size="16" font-weight="700">CW 3</text>
      <rect x="1010" y="100" width="105" height="86" rx="6" fill="{OK}" opacity="0.85"/>
      <text x="1062" y="140" text-anchor="middle" fill="#fff" font-size="16" font-weight="700">CW 4</text>
      <text x="1005" y="205" text-anchor="middle" font-size="13" fill="{MUT}">segment 2 · +&nbsp;pilotes</text>

      <!-- callouts: which parts are QPSK vs the target modulation, and why -->
      <text x="360" y="235" text-anchor="middle" font-size="15" fill="{ACC}" font-weight="700">En QPSK (robuste, module constant, lisible AVANT de connaître le profil)</text>
      <text x="360" y="256" text-anchor="middle" font-size="13" fill="{MUT}">préambule + Marker 1 + Marker 2 — le marker porte justement le profil</text>
      <text x="907" y="235" text-anchor="middle" font-size="15" fill="{OK}" font-weight="700">Dans la modulation CIBLE du profil (ex. 16-APSK)</text>
      <text x="907" y="256" text-anchor="middle" font-size="13" fill="{MUT}">warmup + 4 codewords + pilotes — profil déjà lu dans le marker</text>
    </svg>
  </div>
</section>
"""

# slide 25 — why each frame part exists (present tense, technical rationale)
b[36] = """
<section class="slide">
  <div class="eyebrow">Partie 2 · La trame</div>
  <h2>Pourquoi chaque brique est là</h2>
  <div class="body">
    <ul>
      <li><strong>Préambule</strong> (256 QPSK)&nbsp;: motif connu à module constant → le RX le <strong>corrèle</strong> pour repérer le début, caler l'amplitude et mesurer l'horloge, même noyé dans le bruit.</li>
      <li><strong>Marker d'amorçage</strong> (128 sym)&nbsp;: un sync de 32&nbsp;symboles <em>auto-égalisant</em> + un payload <strong>Golay(24,12)&nbsp;+&nbsp;CRC8</strong>. Il est <strong>décodable à froid, indépendamment du profil</strong>, et porte le <strong>profile_index</strong> + les flags (META / LAST / EOT). C'est lui qui dit au RX <em>quelle constellation</em> il va lire. Il y a <strong>2 markers par période</strong> (un avant chaque segment).</li>
      <li><strong>Warmup</strong> (32 sym) placé <strong>après</strong> le marker&nbsp;: comme le profil est déjà lu, l'égaliseur FFE s'entraîne d'emblée sur la <strong>bonne constellation</strong>.</li>
      <li><strong>Choix de modulation</strong>&nbsp;: préambule et markers sont en <strong>QPSK</strong> — fixe, à module constant, <strong>décodable avant de connaître le profil</strong> (et robuste même sur un profil 16-/64-APSK). Logique&nbsp;: c'est le marker qui <em>porte</em> le profil, il ne peut donc pas être dans la modulation cible. En revanche <strong>warmup, codewords et pilotes sont dans la modulation cible du profil</strong>, une fois celui-ci lu.</li>
      <li><strong>Le marker se répète avant chaque segment</strong> (~toutes les 2 codewords)&nbsp;: chaque segment est <strong>auto-descriptif</strong> → un RX qui arrive en cours de route se raccroche en &lt;&nbsp;4&nbsp;s, sans attendre le début.</li>
      <li><strong>3 familles de préambules (A / B / C)</strong>&nbsp;: des motifs distincts qui évitent qu'un profil se verrouille par erreur sur un autre.</li>
    </ul>
  </div>
</section>
"""

# slide 26 — TX pipeline rebuilt: adds the optional scrambler between RaptorQ and LDPC
_TX_PIPE = [
    ("Fichier source", "image, texte, binaire", INK, False),
    ("Compression", "AVIF · zstd · passthrough", ACC2, False),
    ("AppHeader + CRC", "type MIME, taille, mode", ACC2, False),
    ("RaptorQ encode", "K source + 3–5 % repair", ACC, False),
    ("Scrambler", "whitening G3RUH", WARN, True),
    ("LDPC encode", "WiMAX N=2304 · 1/2 → 5/6", ACC, False),
    ("Mapping symboles", "QPSK / 8-PSK / APSK", OK, False),
    ("Mise en trame V4", "préambule + marker₀ + warmup + pilotes", OK, False),
    ("Filtre RRC (β = 0,20)", "mise en forme bande limitée", OK, False),
    ("Audio 48 kHz", "→ sortie BF radio / carte son / SDR", INK, False),
]
b[37] = f"""
<section class="slide">
  <div class="eyebrow">Chapitre 08 · TX</div>
  <h2>Le pipeline d'émission</h2>
  <div class="two-col two-col-1-2">
    <div class="col">
      <ul>
        <li>Encodage <strong>entièrement en RAM</strong> dans le processus GUI (in-process, <span class="mono">encode_to_samples</span>)</li>
        <li>Le <strong>scrambler</strong> (whitening G3RUH) est <strong>optionnel</strong> — inséré entre RaptorQ et LDPC ; il blanchit le spectre mais peut être coupé pour dialoguer avec un correspondant non blanchi.</li>
        <li>Chaque étage est traçable : on peut sauver l'IQ ou un WAV intermédiaire pour diagnostic</li>
        <li><strong>PTT</strong> déclenché par le worker, séquencement TX → audio → RX</li>
        <li>Pas de retransmission ni d'ARQ — émission <em>one-shot</em> avec la couche de repair RaptorQ</li>
      </ul>
    </div>
    <div class="col">
      {vchain("Chaîne d'émission", _TX_PIPE)}
    </div>
  </div>
</section>
"""

# ch9 divider — flag it as the first-generation decoder, now being replaced by turbo
b[38] = b[38].replace(
    "Premier décodeur : prendre tout le WAV, chercher partout, décoder.",
    "La première version du décodeur (batch, sur tout le WAV) — aujourd'hui en cours de remplacement par le récepteur turbo streaming. On la garde pour le décodage hors-ligne de fichiers WAV.")

# ch9 batch pipeline — rebuilt with an optional descrambler after LDPC
_RX_BATCH = [
    ("Preamble\nprobe", "détection"), ("Sync\nmarker", "profil"), ("DD-PLL", "phase"),
    ("FFE", "égaliseur"), ("Soft\ndemap", "→ LLR"), ("LDPC", "décode"),
    ("Descramble", "optionnel"), ("RaptorQ", "→ fichier"), ("Décompress.", "audio/image"),
]
_rx_boxes = []
_bw, _bh, _gap, _x0 = 118, 54, 18, 20
for _i, (_a, _sub) in enumerate(_RX_BATCH):
    _x = _x0 + _i * (_bw + _gap)
    _col = ACC if _i in (0,) else (OK if _i in (5, 7) else (WARN if _i == 6 else ACC2))
    _dash = ' stroke-dasharray="7 4"' if _i == 6 else ''
    _lines = _a.split("\n")
    _ty = 100 - (len(_lines) - 1) * 8
    _rx_boxes.append(f'<rect x="{_x}" y="80" width="{_bw}" height="{_bh}" rx="6" fill="{_col}" opacity="0.16" stroke="{_col}" stroke-width="2"{_dash}/>')
    for _j, _ln in enumerate(_lines):
        _rx_boxes.append(f'<text x="{_x+_bw/2:.0f}" y="{_ty+_j*15}" text-anchor="middle" font-size="12" font-weight="700" fill="{INK}">{_ln}</text>')
    _rx_boxes.append(f'<text x="{_x+_bw/2:.0f}" y="{80+_bh-8}" text-anchor="middle" font-size="10" fill="{MUT}">{_sub}</text>')
    if _i < len(_RX_BATCH) - 1:
        _ax = _x + _bw
        _rx_boxes.append(f'<line x1="{_ax}" y1="107" x2="{_ax+_gap}" y2="107" stroke="{INK}" stroke-width="1.5"/>')
        _rx_boxes.append(f'<polygon points="{_ax+_gap},107 {_ax+_gap-7},103 {_ax+_gap-7},111" fill="{INK}"/>')
_rx_w = _x0 + len(_RX_BATCH) * (_bw + _gap)
b[39] = f"""
<section class="slide">
  <div class="eyebrow">Chapitre 09 · Batch RX (1ʳᵉ version)</div>
  <h2>Pipeline batch (rx_v2) — en cours de remplacement</h2>
  <div class="full-image">
    <svg viewBox="0 0 {_rx_w} 170" style="max-height:44vmin;">
      <text x="40" y="55" font-size="12" fill="{MUT}">audio in (48 kHz)</text>
      {''.join(_rx_boxes)}
    </svg>
  </div>
  <div class="caption">Un seul pass sur tout le buffer. Le <strong>descrambler</strong> (après LDPC) est optionnel, comme le scrambler TX. Le récepteur <strong>turbo</strong> (plus loin) remplace ce chemin en streaming.</div>
</section>
"""

# slide 29 — clarify the drifting clocks are the A/D & D/A converter clocks
b[40] = b[40].replace(
    "L'horloge TX et l'horloge RX dérivent — typiquement quelques dizaines de ppm",
    "Les horloges des convertisseurs <strong>A/D (RX) et D/A (TX)</strong> — les quartz des cartes son — dérivent l'une vis-à-vis de l'autre, typiquement quelques dizaines de ppm")

# ch10 streaming — clarify the low-power (Pi) mode runs a REDUCED grid
b[47] = b[47].replace(
    "<li>Le <strong>marker tous les 4 s</strong> permet de rattraper une trame en cours</li>",
    "<li>Le <strong>marker (~4 s, soit ≈ 4 codewords sur les profils rapides)</strong> permet de rattraper une trame en cours</li>")
b[47] = b[47].replace(
    "<li>Mode Power : large grille seulement au démarrage, puis on suit en local</li>",
    "<li>Sur matériel contraint (<strong>mode low-power</strong>, Raspberry&nbsp;Pi)&nbsp;: <strong>grille de recherche réduite</strong> — large seulement à l'acquisition, puis on suit localement pour économiser le CPU</li>")

# FIR slide — define the three acronyms (FIR / ISI / LMS)
b[42] = b[42].replace(
    "<li>FIR = <em>Finite Impulse Response</em>. Pas d'analogue analogique direct, mais on peut le voir comme une <strong>ligne à retard à dérivations</strong> avec un sommateur</li>",
    "<li><strong>FIR</strong> = <em>Finite Impulse Response</em> (réponse impulsionnelle finie) : un filtre vu comme une <strong>ligne à retard à dérivations</strong> + un sommateur.</li>"
    "<li><strong>ISI</strong> = <em>Inter-Symbol Interference</em> : les échos du canal font <strong>baver</strong> chaque symbole sur ses voisins → le filtre sert à la <strong>compenser</strong>.</li>"
    "<li><strong>LMS</strong> = <em>Least Mean Squares</em> : la méthode qui <strong>apprend</strong> les coefficients en minimisant l'erreur (détaillée plus loin).</li>")

# DFE slide split in two: (A) FFE-only rationale, (B) the DFE reference diagram
b[43] = """
<section class="slide">
  <div class="eyebrow">Chapitre 09 · Égaliseur</div>
  <h2>FFE seul aujourd'hui, pas de DFE — et pourquoi</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li><strong>FFE</strong> = <em>Feed-Forward Equalizer</em> : un FIR sur le <strong>signal reçu</strong>.</li>
        <li><strong>DFE</strong> = <em>Decision-Feedback Equalizer</em> : FFE + un second FIR alimenté par les <strong>décisions</strong> déjà prises (retour décisionnel).</li>
        <li><strong>Le récepteur turbo n'utilise que le FFE</strong> — pas de retour décisionnel.</li>
        <li><em>Pourquoi&nbsp;?</em> Le retour de la DFE réinjecte des <strong>décisions dures</strong>&nbsp;: à bas SNR une décision fausse <strong>se propage</strong> dans la boucle.</li>
        <li>La <strong>boucle turbo</strong> obtient le même gain autrement&nbsp;: elle réinjecte des <strong>symboles souples</strong> (probabilités) — donc pas de propagation d'erreur.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 380 300" style="max-height:52vmin;">
        <rect x="0" y="0" width="380" height="300" fill="#fff" stroke="#e6e2d8" stroke-width="2"/>
        <text x="190" y="30" text-anchor="middle" font-size="14" font-weight="700">FFE seul</text>
        <rect x="60" y="55" width="120" height="50" rx="8" fill="#2a5f8a" opacity="0.15" stroke="#2a5f8a" stroke-width="2"/>
        <text x="120" y="85" text-anchor="middle" font-size="14" font-weight="700" fill="#2a5f8a">FFE</text>
        <line x1="180" y1="80" x2="250" y2="80" stroke="#1a1a1a" stroke-width="2"/>
        <text x="300" y="85" text-anchor="middle" font-size="13" fill="#1a1a1a">→ démap souple</text>
        <text x="190" y="150" text-anchor="middle" font-size="13" font-weight="700" fill="#b8412a">DFE = + retour décisionnel</text>
        <rect x="60" y="175" width="120" height="46" rx="8" fill="#b8412a" opacity="0.12" stroke="#b8412a" stroke-width="2" stroke-dasharray="6 4"/>
        <text x="120" y="203" text-anchor="middle" font-size="13" font-weight="700" fill="#b8412a">FB (décisions)</text>
        <path d="M 250 200 C 230 245 150 245 130 223" fill="none" stroke="#b8412a" stroke-width="2" stroke-dasharray="6 4"/>
        <text x="230" y="262" text-anchor="middle" font-size="11" fill="#b8412a">décision dure → risque de propagation</text>
        <line x1="200" y1="240" x2="200" y2="240" stroke="#b8412a"/>
      </svg>
    </div>
  </div>
</section>
"""

B43B = """
<section class="slide">
  <div class="eyebrow">Chapitre 09 · Pour mémoire</div>
  <h2>La DFE classique : deux FIR, un retour décisionnel</h2>
  <div class="two-col">
    <div class="col">
      <ul>
        <li>La DFE empile <strong>deux FIR</strong> opposés :
          <ul>
            <li><strong>FFE</strong> (forward) sur les échantillons reçus — corrige l'ISI des symboles <em>à venir</em></li>
            <li><strong>FB</strong> (feedback) sur les symboles <em>déjà décidés</em> — annule l'ISI des symboles passés</li>
          </ul>
        </li>
        <li>Le <strong>slicer</strong> choisit le symbole le plus proche ; sa décision repart dans le FB → <strong>boucle fermée</strong>.</li>
        <li>Avantage : le FB travaille sur du <strong>signal propre</strong> (symboles décidés).</li>
        <li>Inconvénient : une <strong>décision fausse se propage</strong> — la raison du choix FFE-seul + turbo.</li>
      </ul>
    </div>
    <div class="col">
      <svg viewBox="0 0 520 360" style="max-height: 56vmin;">
        <defs>
          <marker id="dfe2-arr" markerWidth="6" markerHeight="6" refX="6" refY="3" orient="auto"><polygon points="0,0 6,3 0,6" fill="#1a1a1a"/></marker>
          <marker id="dfe2-arr-r" markerWidth="6" markerHeight="6" refX="6" refY="3" orient="auto"><polygon points="0,0 6,3 0,6" fill="#b8412a"/></marker>
        </defs>
        <text x="15" y="100" font-size="13" fill="#1a1a1a">x(n)</text>
        <line x1="50" y1="95" x2="80" y2="95" stroke="#1a1a1a" stroke-width="1.5" marker-end="url(#dfe2-arr)"/>
        <rect x="80" y="60" width="160" height="70" rx="6" fill="#2a5f8a"/>
        <text x="160" y="92" text-anchor="middle" font-size="14" fill="#fff" font-weight="700">FFE</text>
        <text x="160" y="112" text-anchor="middle" font-size="11" fill="#fff">FIR sur l'entrée</text>
        <line x1="240" y1="95" x2="290" y2="95" stroke="#1a1a1a" stroke-width="1.5" marker-end="url(#dfe2-arr)"/>
        <circle cx="305" cy="95" r="15" fill="#fff" stroke="#1a1a1a" stroke-width="1.5"/>
        <text x="305" y="100" text-anchor="middle" font-size="14" font-weight="700">+</text>
        <text x="305" y="80" text-anchor="middle" font-size="11" fill="#b8412a" font-weight="700">−</text>
        <line x1="320" y1="95" x2="370" y2="95" stroke="#1a1a1a" stroke-width="1.5" marker-end="url(#dfe2-arr)"/>
        <rect x="370" y="60" width="100" height="70" rx="6" fill="#2f7a3a"/>
        <text x="420" y="92" text-anchor="middle" font-size="14" fill="#fff" font-weight="700">Slicer</text>
        <text x="420" y="112" text-anchor="middle" font-size="11" fill="#fff">décision dure</text>
        <line x1="470" y1="95" x2="510" y2="95" stroke="#1a1a1a" stroke-width="1.5"/>
        <text x="515" y="100" font-size="13" fill="#1a1a1a" font-weight="700">d̂(n)</text>
        <line x1="490" y1="95" x2="490" y2="220" stroke="#b8412a" stroke-width="1.5"/>
        <line x1="490" y1="220" x2="370" y2="220" stroke="#b8412a" stroke-width="1.5" marker-end="url(#dfe2-arr-r)"/>
        <rect x="210" y="190" width="160" height="70" rx="6" fill="#b8412a"/>
        <text x="290" y="222" text-anchor="middle" font-size="14" fill="#fff" font-weight="700">FB</text>
        <text x="290" y="242" text-anchor="middle" font-size="11" fill="#fff">FIR sur les décisions</text>
        <line x1="210" y1="225" x2="305" y2="225" stroke="#b8412a" stroke-width="1.5"/>
        <line x1="305" y1="225" x2="305" y2="113" stroke="#b8412a" stroke-width="1.5" marker-end="url(#dfe2-arr-r)"/>
        <text x="260" y="335" text-anchor="middle" font-size="12" fill="#555">FFE et FB ont chacun leurs taps et leur LMS propre.</text>
      </svg>
    </div>
  </div>
</section>
"""

# slide 44 — QO-100 as a FORWARD target: exploit the linear channel further
b[63] = """
<section class="slide">
  <div class="eyebrow">Projets futurs · QO-100</div>
  <h2>Exploiter à fond le canal linéaire QO-100</h2>
  <div class="body">
    <ul>
      <li>Le lien <strong>QO-100 SSB</strong> fonctionne déjà (Pluto TX+RX, récepteur turbo). Le canal est <strong>linéaire</strong>&nbsp;: pas de limiteur, pas de désemphase.</li>
      <li>La largeur par station est bridée à <strong>2,7 kHz</strong> par la réglementation — mais ces 2,7 kHz sont <strong>utilisables à 100 %</strong>, linéairement.</li>
      <li>Piste&nbsp;: <strong>constellations plus denses</strong> (APSK / QAM) et <strong>débits plus élevés</strong>, que l'écrêtage du relais FM interdisait.</li>
      <li>Piste&nbsp;: <strong>OFDM</strong> redevient envisageable sur ce canal linéaire (diversité fréquentielle sans PAPR fatale).</li>
      <li>À affiner encore&nbsp;: poursuite <strong>continue</strong> de la dérive Doppler / OL et du bruit de phase à 10 GHz sur les longues transmissions.</li>
    </ul>
  </div>
</section>
"""

# SDR crates table — list supported SDRplay models
b[56] = b[56].replace(
    "<tr><td>modem-sdrplay</td><td>SDRplay RSPduo (RX)</td><td>bindgen dynamic lib</td></tr>",
    "<tr><td>modem-sdrplay</td><td>SDRplay <strong>RSPduo · RSPdx · RSPdx-R2</strong> (RX)<br><span style=\"font-size:0.85em;color:var(--muted)\">autres modèles sur demande</span></td><td>bindgen dynamic lib</td></tr>")

# SDR RX pipeline — add the FM / SSB demodulator choice
b[57] = f"""
<section class="slide">
  <div class="eyebrow">Chapitre 13 · SDR</div>
  <h2>Pipeline RX SDR — FM ou SSB au choix</h2>
  <div class="full-image">
    <svg viewBox="0 0 1200 340" style="max-height:52vmin;">
      <defs><marker id="a4" markerWidth="6" markerHeight="6" refX="6" refY="3" orient="auto"><polygon points="0,0 6,3 0,6" fill="{INK}"/></marker></defs>
      <g font-size="13" text-anchor="middle">
        <rect x="20" y="145" width="150" height="50" rx="6" fill="{ACC}"/>
        <text x="95" y="175" fill="#fff" font-weight="700">SDR (I/Q)</text>
        <text x="95" y="215" font-size="11" fill="{MUT}">RTL-SDR / Pluto / SDRplay</text>

        <!-- branch: two demodulators -->
        <rect x="240" y="70" width="230" height="52" rx="6" fill="{ACC2}"/>
        <text x="355" y="92" fill="#fff" font-weight="700">Démod FM (quadrature)</text>
        <text x="355" y="110" fill="#cfe1ee" font-size="11">NBFM → audio mono</text>

        <rect x="240" y="218" width="230" height="52" rx="6" fill="{OK}"/>
        <text x="355" y="240" fill="#fff" font-weight="700">Démod SSB (USB-data)</text>
        <text x="355" y="258" fill="#c8e0cc" font-size="11">QO-100 — translation</text>

        <rect x="520" y="70" width="180" height="52" rx="6" fill="{ACC2}"/>
        <text x="610" y="100" fill="#fff" font-weight="700">Dé-emphase 6 dB/oct</text>
        <text x="610" y="150" font-size="11" fill="{MUT}">(FM uniquement)</text>

        <rect x="750" y="145" width="170" height="50" rx="6" fill="{ACC2}"/>
        <text x="835" y="175" fill="#fff" font-weight="700">Resample → 48 kHz</text>

        <rect x="960" y="145" width="140" height="50" rx="6" fill="{OK}"/>
        <text x="1030" y="175" fill="#fff" font-weight="700">Modem turbo</text>

        <g stroke="{INK}" stroke-width="1.5" fill="none" marker-end="url(#a4)">
          <line x1="170" y1="160" x2="240" y2="96"/>
          <line x1="170" y1="180" x2="240" y2="244"/>
          <line x1="470" y1="96" x2="520" y2="96"/>
          <line x1="700" y1="96" x2="835" y2="143"/>
          <line x1="470" y1="244" x2="835" y2="197"/>
          <line x1="920" y1="170" x2="960" y2="170"/>
        </g>
        <text x="205" y="60" font-size="12" fill="{MUT}" font-weight="700">2 démod. au choix</text>
      </g>
    </svg>
  </div>
  <div class="caption">Tout le DSP est dans <span class="mono">modem-sdr-dsp</span> (partagé). La dé-emphase ne s'applique qu'en FM ; en SSB/QO-100 le canal est linéaire.</div>
</section>
"""

# demo slide — kept generic (profile varies by site); tonight's plan noted
b[65] = """
<section class="slide statement">
  <div>
    <div class="big" style="font-size: 14vmin; color: var(--accent);">Démo</div>
    <div class="small" style="font-size:2.6vmin;">
      En direct — modes et débits <strong>selon le site et la propagation</strong>.<br>
      Ce soir&nbsp;: le <strong>relais de La Praz</strong> dans différents modes,<br>
      et très probablement <strong>QO-100</strong>.
    </div>
  </div>
</section>
"""

# slide 47 — récap rewritten: turbo / QO-100 / SDR moved to "done"
b[66] = """
<section class="slide">
  <div class="eyebrow">Récap</div>
  <h2>Où on en est</h2>
  <div class="two-col">
    <div class="col">
      <div style="font-size: 2.5vmin; color: var(--ok); font-weight: 700; margin-bottom: 1vmin;">✓ Ce qui marche</div>
      <ul style="font-size: 2.1vmin;">
        <li>10 profils opérationnels (QPSK → 64-APSK)</li>
        <li>LDPC + RaptorQ stables</li>
        <li>Trame à segments cycliques (re-synchro ~4 s)</li>
        <li><strong>Récepteur turbo</strong> streaming (CFO / SFO / boucle turbo)</li>
        <li><strong>Scrambler</strong> (spectre blanchi)</li>
        <li>SDR : RTL-SDR, SDRplay, PlutoSDR</li>
        <li><strong>QO-100 SSB</strong> : Pluto TX + RX</li>
        <li>GUI Tauri, CLI, multi-plateformes · Pi 4 / aarch64</li>
      </ul>
    </div>
    <div class="col">
      <div style="font-size: 2.5vmin; color: var(--warn); font-weight: 700; margin-bottom: 1vmin;">⚙ À venir</div>
      <ul style="font-size: 2.1vmin;">
        <li>QO-100 : constellations plus denses / débits plus élevés</li>
        <li>OFDM revisité sur canal linéaire</li>
        <li>HF / SSB (NVIS, DX)</li>
        <li>Poursuite continue de dérive sur longues salves</li>
      </ul>
    </div>
  </div>
</section>
"""

# closing slide
FINAL_SLIDE = """
<section class="slide statement">
  <div>
    <div class="big" style="font-size: 11vmin; color: var(--accent);">73&nbsp;!</div>
    <div class="small" style="font-size:2.6vmin; margin-top: 3vmin;">
      Merci à <strong style="color:var(--ink)">HB9MM</strong>.<br>
      <strong style="color:var(--ink)">HB9TOB</strong> · projet open source<br>
      <span class="mono" style="font-size:2vmin;">github.com/hb9tob/NewModem</span>
    </div>
  </div>
</section>
"""

# very last slide (closes the annex too)
END_SLIDE = """
<section class="slide statement">
  <div>
    <div class="big" style="font-size: 12vmin; color: var(--accent);">Merci&nbsp;!</div>
    <div class="small" style="font-size:2.6vmin; margin-top: 3vmin;">
      Questions, essais, contacts bienvenus.<br>
      <strong style="color:var(--ink)">HB9TOB</strong> · <strong style="color:var(--ink)">73&nbsp;!</strong><br>
      <span class="mono" style="font-size:2vmin;">github.com/hb9tob/NewModem</span>
    </div>
  </div>
</section>
"""

order = []
# intro
order += [title_slide, NEW_TOC, b[2], b[3]]
# PART 1 - primer (+ SDR / QO-100 + turbo teaser)
order += [PRIMER_DIVIDER, P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P10B, P10C, P10D, P_SDR, P_TURBO_TEASER]
# stop point
order += [STOP]
# PART 2 - under the hood
order += [ESSENTIAL_DIVIDER, E_TX, E_RX, E_NCO]
# frame — current V4 only (b[34] legacy dropped): divider, diagram, why, bridge, TX pipeline
order += [b[33], b[35], b[36], FRAME_BRIDGE, b[37]]
# equalisation concept slides (FIR / DFE split in 2 / DFE-training / LMS)
order += [b[38], b[39], b[40], b[42], b[43], B43B, b[44], b[41]]
order += b[45:48]            # streaming FFE (existing)
# the turbo receiver: overview block diagram + soft-bit primer + loop + CFO/SFO/scrambler + recap
order += [TURBO_DIVIDER, TURBO_BLOCKDIAG, SOFTBIT_SLIDE, TURBO_CORE, CFO_SLIDE, SFO_SLIDE, SCRAMBLER_SLIDE, FLYWHEEL_SLIDE, TURBO_RECAP]
order += b[21:26]            # constellations retenues (existing)
# future (turbo now shipped → dropped from future; QO-100/HF remain)
order += [b[61], b[63], b[64]]
order += b[65:68]            # demo, récap, Questions?
order += [FINAL_SLIDE]       # closes the main talk
# ANNEX
order += [ANNEX_DIVIDER]
order += b[4:7]              # RustPIC OFDM
order += b[7:12]             # canal
order += b[12:16]            # APSK/FTN
order += b[16:21]            # pilotes
order += b[26:30]            # LDPC/RaptorQ detailed
order += b[30:33]            # couche applicative detailed
order += b[51:55]            # sondeur
order += b[55:59]            # SDR
order += b[59:61]            # low-power
order += [END_SLIDE]         # very last slide

body = "".join(order)

# ---------------------------------------------------------------------------
# rebrand head + tail
# ---------------------------------------------------------------------------
head_new = head.replace("Modem NBFM — HB9LC, mai 2026",
                        "Modem NBFM — HB9MM, juillet 2026")
# inject the rotating-phasor keyframes used by the primer (P4)
head_new = head_new.replace(
    "</style>",
    "@keyframes spin { from { transform: rotate(0deg); } to { transform: rotate(360deg); } }\n</style>")
tail_new = tail.replace("Modem NBFM · HB9LC 2026", "Modem NBFM · HB9MM")

out = head_new + body + tail_new
open(OUT, "w", encoding="utf-8").write(out)

# report
n_slides = out.count('<section class="slide')
print(f"wrote {OUT}: {n_slides} slides, {len(out)} bytes")
