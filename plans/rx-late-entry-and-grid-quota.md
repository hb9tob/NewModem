# RX late-entry recovery + lowpower grid quota

**Date** : 2026-05-15 (soirée)
**Versions livrées** : 0.10.33 (late-entry), 0.10.34 (grid quota — buggé,
voir « bug connu » plus bas)
**Hardware testé** : Pi 4 + FT-991A (TX, PC) → OTA → FT-DX1 → soundcard
USB du Pi 4 (Pi 4 = RX)

---

## Symptôme initial

Sur transmission OTA HIGH+ de 120 blocs RaptorQ, après ~5 SFs décodées
correctement le RX entrait dans une phase « infernale » :

- LDPC + Gardner consommaient massivement le CPU pendant **plusieurs
  minutes** (perçu utilisateur).
- La GUI passait en rouge (lag > 300 ms).
- Le `[rx_v2 gardner] only 2 markers found, need >=3` se répétait en
  boucle dans `/dev/shm/rx-trace.log`.
- 0.9 décodait ce même scénario sans souci → régression introduite
  par 0.10.x lowpower.

## Diagnostic

1. **Gardner instable sur SF marginale**. Sur le trace, ppm Gardner
   = -100/-100/-107/**-141**/-112/-107/-107. Le saut à -141 (vs cluster
   à -105) est un outlier (sub-symbol misalignement, faux positif
   marker) → resample SF suivante au mauvais ppm → markers SF+1 hors
   fenêtre de recherche → cascade « only 2 markers ».

2. **Aucun mécanisme de récupération**. La trame en cours bloque le
   worker. Le `preamble_silence_timeout` ne fire pas car le buffer
   contient encore le préambule de la SF précédente. Pas de saut au
   prochain préambule prédit.

3. **Grille ±15 ppm désactivée** en lowpower (`allow_legacy_grid =
   false` sur aarch64) → CW marginaux qu'elle aurait sauvés en 0.9
   sont définitivement perdus.

4. **Drift réel mesuré** : ~-105 ppm stable, dominé par les codecs USB
   des transceivers (pas TCXO-locké RF — le crystal du codec USB est
   séparé du TCXO). 0.9 absorbait ce drift via FFE-LMS + pilot
   tracker car pas de Gardner pour le décoder activement, donc pas
   d'outlier à propager.

---

## 0.10.33 — Late-entry recovery (lowpower only)

`rust/modem-worker/src/rx_worker.rs`, constantes ~525, état dans
`WorkerState` ~599, logique en fin de `scan_and_route` ~1709.

**Principe** : quand un cluster de N ticks consécutifs ne produit
aucun CW pendant une session active, prédire la position absolue du
prochain préambule via la cadence V3 (`V3_PREAMBLE_PERIOD_S = 4.0 s`)
relative à l'ancre du dernier décodage réussi. Drainer le buffer
jusqu'à `predicted - margin`, clear `session_drift_ppm` → Gardner
repart frais sur audio non-contaminé.

**Paramètres** :
- `LATE_ENTRY_FAIL_THRESHOLD = 3` ticks (~600 ms)
- `LATE_ENTRY_MARGIN_MS = 200`

**État ajouté** :
- `last_decoded_preamble_audio_pos: Option<u64>` (position absolue
  cumulative en samples du préambule du dernier décodage réussi).
- `n_consecutive_undecoded_ticks: u32`.

**Reset** : `soft_reset_buffer()` et `trim_buffer_to_preroll()`.

**Validation OTA** : la cascade « infernal » de plusieurs minutes est
réduite à un saut isolé de ~600 ms, après lequel le décodage reprend
normalement.

---

## 0.10.34 — Lowpower grid as last resort + quota (BUG CONNU)

`rust/modem-worker/src/rx_worker.rs`, constantes ~545, état
`lowpower_grid_recent: VecDeque<Instant>` dans `WorkerState`, logique
au début de `scan_and_route` (calcul `effective_allow_grid`) et après
`take_perf()` (incrément du compteur).

**Principe** : en lowpower (`!allow_legacy_grid`), passer
`effective_allow_grid = true` au `rx_v3_after` quand la quota n'est
pas saturée, sinon `false`. La grille ±15 ppm dans `rx_v2_with_options`
ne tourne que comme dernier recours (Gardner + fast-path ont échoué).

**Paramètres** :
- `LOWPOWER_GRID_QUOTA = 2` invocations
- `LOWPOWER_GRID_WINDOW_S = 4` s

**Bug connu** : la détection « grid ran this tick » utilise
`perf_this_tick.n_passes > 2` comme heuristique, mais elle est
imprécise. Cas faux positifs observés :
- Pre-decode Gardner (rx_worker.rs:1153) ajoute +1 PassDone même
  hors grille.
- Combinaison Gardner+hint+fast-path = 3 passes sans grille.
- Après late-entry (`session_drift_ppm = None`), pre-decode Gardner
  re-tourne → +1 pass → quota saturée pour rien.

Dans `/tmp/nbfm-worker.log` on observe `recent=3/2`, `4/2`, etc.
au-delà de la quota nominale. Le gate ne limite pas correctement.

**Fix proposé pour 0.10.35** :
1. **Drop la quota** entièrement (deque, compteur, log
   `[grid-quota]`). Plus de faux positifs, code plus simple.
2. **Early-break dans la grille** (`rx_v2.rs:625-636`) : sortir
   immédiatement de la boucle dès qu'un step produit un décode
   propre (`clean(&r)`). Coupe le coût grille de 2-6× en moyenne
   sans rien sacrifier (le 1er step qui marche est conservé).
3. **Toujours autoriser la grille** en lowpower, gated naturellement
   par `still_not_clean` (déjà en place dans rx_v2.rs).

L'argument : avec early-break, une grille « inutilement complète »
coûte 6× MF (~3.5 s sur Pi 4) ; avec early-break sur SF stable
elle coûte 1-2× MF (~600 ms). Plus de quota nécessaire.

---

## Pending (axe perf indépendant)

**Axis 3 — f32 sur le path matched-filter aarch64** (cf.
`~/.claude/plans/le-fast-path-doit-dapper-neumann.md`). Gain estimé
~2× sur la FFT via NEON 128-bit. Sur Pi 4 :
- `matched_filter` 590 ms → ~290 ms par SF
- Tick total CLOSED : 734 ms → ~440 ms
- Tick avec grille 6 passes : 3.5 s → ~1.7 s

Combiné avec early-break, une SF marginale passerait de plusieurs
secondes à <500 ms typique.

---

## Toolchain Pi 4 (gotcha installé en CLAUDE.md)

`/usr/bin/rustc` (paquet Debian, 1.85.0) est trop vieux pour
`raptorq 2.0.1` (besoin de `unsigned_is_multiple_of` stabilisé en
1.87). `~/.cargo/bin` n'est pas dans `PATH` par défaut → il faut
invoquer cargo via le chemin absolu `/home/hb3xek/.cargo/bin/cargo`
ou sourcer `~/.cargo/env`. Le rustup-managed est à 1.95.0.

---

## Référence trace

Trace OTA du diagnostic :
`results/rx-trace-rpi4-axis12-fix-2212.log` (1260 lignes, 5 SF
décodées avec pattern Gardner stable -105 ppm avant cascade
d'échecs).
