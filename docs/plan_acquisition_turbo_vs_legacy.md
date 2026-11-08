# Plan — acquisition turbo vs legacy : diagnostic chiffré et remédiation

**Date d'ouverture** : 2026-11-08
**Branche** : main (à l'ouverture : 4143cca, v0.20.1)
**État** : étape 1 en cours
**Objet** : sur relais FM, le modem turbo donne des résultats *inférieurs* au
modem legacy — erreurs d'entrée et décochages que le legacy n'a pas — et il
accumule un retard massif en saturant le CPU avant d'entrer, puis rattrape tout
le retard en très peu de temps. Ce document est le plan de diagnostic puis de
remédiation. Il est mis à jour **à chaque étape franchie** (la machine de dev
peut crasher ; rien ne doit vivre uniquement en mémoire de session).

---

## Journal d'avancement

| # | Étape | État | Date | Sortie |
|---|-------|------|------|--------|
| 0 | Analyse des deux chaînes d'acquisition dans le code | **fait** | 2026-11-08 | section « Analyse statique » ci-dessous |
| 1 | Énumérer les scénarios + figer les métriques | **en cours** | — | sections « Scénarios » + « Métriques » |
| 2 | Générer une transmission test sans blocs RaptorQ supplémentaires | à faire | — | `--repair-pct 0` |
| 3 | Adapter le simulateur de canal aux scénarios | à faire | — | patch `study/nbfm_channel_sim.py` |
| 4 | Générer les fichiers audio des scénarios | à faire | — | `results/acq_scenarios/*.wav` |
| 4.5 | Harness WAV→turbo + instrumentation symétrique | à faire | — | sous-commande CLI + profileur turbo |
| 5 | Décoder avec les 2 modems, mesurer étapes internes et temps | à faire | — | tableau de mesures |
| 6 | Plan de remédiation avec objectifs chiffrés | à faire | — | section « Remédiation » |
| 7 | Découpage en sessions et exécution | à faire | — | — |

Règle : à chaque étape franchie, mettre à jour la ligne (état + date + sortie)
et ajouter les résultats dans la section correspondante.

---

## Symptômes observés (terrain)

1. Sur relais FM, le turbo rate des entrées que le legacy réussit.
2. Le turbo décroche en cours de transmission ; le legacy ne décroche pas.
3. Avant d'entrer, le turbo prend un retard considérable en consommant
   énormément de CPU, puis rattrape tout le retard en très peu de temps.

---

## Analyse statique (étape 0 — faite)

### Les deux chaînes

| | Legacy | Turbo |
|---|---|---|
| Boucle worker | `rx_worker.rs:1661` `maintenance_tick` → `scan_and_route`, tick 1 s | `rx_worker.rs:994` `run_turbo_worker`, piloté par les échantillons |
| Détecteur d'entrée | `gate.rs` `PreambleProbe` + `sync.rs:92` `find_all_preambles` | `turbo_gate.rs:142` `TurboGate` + `v3_session.rs:1762` `try_acquire_preamble` |
| Décodeur | `rx_v2.rs:1969` `rx_v3_after` — fenêtre, sans état | `v3_session.rs` `V3Session` — streaming, avec état |
| SFO | `rx_v2.rs:1008` Gardner + grille ±15 / ±80 ppm | `v3_session.rs:95` `PreambleSfoTracker`, boucle fermée |

Différence de principe : **legacy est sans mémoire** (chaque tick repart de
zéro, le même préambule est réessayé `N_positions × N_ppm × N_ticks` fois, les
seuils sont *relatifs*) ; **turbo est à état** (un seul candidat, un seul essai,
seuils *absolus*, engagements one-shot, chaîne de prédiction avec abandon à
3 markers ratés).

### Causes identifiées, classées

| # | Cause | Preuve | Symptôme |
|---|-------|--------|----------|
| ① | Seuil de détection **absolu** sur une métrique fraction-d'énergie, gabarit plat, audio passe-bande brut : `metric = corr²/(E_template·E_fenêtre) ≥ 0.15` | `fd_acquire.rs:35`, `gate_mf.rs:167` | n'entre pas du tout |
| ② | **Aucune acquisition SFO grossière.** Boucle de timing active par défaut (`v3_session.rs:1039`) ⇒ Gardner / 2-préambules / marker-LS tous court-circuités (`v3_session.rs:3566`) ; et `drift_ppm` est **ignoré** par le DSP quand la boucle est active (`streaming_dsp.rs:204`). `run_timing_acquisition`, cité par le commentaire, **n'existe pas dans l'arbre** : seul « track » a été écrit, pas « acquire ». | — | 1er superframe (4 s) à 0 ppm, entrée dégradée |
| ③ | `MAX_CONSECUTIVE_MISS = 3` → `SessionLost` + `finalize`, puis re-détection **à froid** complète (driver détruit + `TurboGate::auto()` neuf) | `v3_session.rs:232`, `:3428`, `rx_worker.rs:1294` | décochages, jusqu'à 4 s perdues par décrochage |
| ④ | Candidat unique (max global) et **un seul** essai de validation du marker | `v3_session.rs:1935` | entrées manquées intermittentes |
| ⑤ | Self-throttle `max(200 ms, 3×wall_poll)` alors que la fenêtre ne couvre que `template + 450 ms`, et le reste du ring n'est jamais relu ⇒ **tout poll > 150 ms crée un trou d'audio jamais examiné**. Idem phase 2 : cadence 800 ms contre 450 ms de couverture ⇒ ~350 ms de positions invisibles (le commentaire `turbo_gate.rs:56` affirme le contraire) | `turbo_gate.rs:397` | latence d'acquisition, entrées manquées sur hôte lent |
| ⑥ | Engagements one-shot non révisables : `cfo_locked`, `drift_locked` posé même sans application, Étage B plafonné à 4 et désactivé par défaut | `v3_session.rs:1842`, `:3744` | burst entier faussé après une mauvaise entrée |
| ⑦ | DD-LMS avant sur décisions dures — échec déjà documenté dans le code : *« turbo < legacy on fading »* | `v3_session.rs:300` | dégradation en fading |

### Mécanisme du retard massif + rattrapage brutal

Le retard vit dans le `mpsc::channel()` **non borné** entre le thread lecteur et
le worker (`cpal_capture.rs:163` ; le lecteur vide l'anneau de 30 s quoi qu'il
arrive, donc **aucun échantillon n'est perdu** — le retard s'accumule en RAM,
~192 ko par seconde de retard). `lag_ms` = `wall_s − total_samples/48000` est
exactement la taille de ce backlog.

La recherche coûte **plus** cher que la réception, parce que la phase 2 CFO ne
tourne **que** quand la phase 1 échoue (`turbo_gate.rs:316`) :

| état | travail par poll |
|---|---|
| phase 1 seule | 3 géométries × (1 aller + 1 retour) = **6 FFT** (65536 / 65536 / 131072) |
| phase 1 échoue + énergie en bande | + `coarse_psd` 16384 + 3 × (1 aller + 11 retours) = **~37 FFT** |
| verrouillé | **0** — gate abandonné, `try_acquire_preamble` sort 1ʳᵉ ligne (`v3_session.rs:1763`) |

`refine_grid` = 11 points (`cfo.rs:74`). La boucle est auto-entretenue : métrique
< 0.15 → phase 1 vide → 37 FFT / 800 ms → CPU saturé → retard → self-throttle
allonge → trous de couverture → toujours pas d'entrée.

Trois amplificateurs :

- **Le DSP « parqué » n'est pas actif par défaut** : `gated: std::env::var_os("TURBO_GATED").is_some()` (`rx_v3_worker.rs:250`) ⇒ toute l'architecture « gated turbo » prévue pour parquer resampler + MF + FFE pendant la recherche est désactivée.
- **L'acquisition in-session paie ~10× le prix nécessaire** : `v3_session.rs:1808` boucle `best_match_derotated` sur les 11 points (11 aller + 11 retour, en f64) alors que `fd_acquire.rs:730` `best_match_cfo_grid` fait la même chose avec **un seul** aller partagé — et c'est déjà ce que le gate utilise.
- **Pas d'amortisseur sur le chemin in-session** : le self-throttle wall-clock n'existe que dans `TurboGate`. `try_acquire_preamble` n'est freinée qu'en espace-échantillon ⇒ en rattrapage, le nombre de polls **par seconde murale** explose ⇒ rétroaction positive.

Rattrapage : une fois verrouillé, il ne reste que le `StreamingDsp` (banc
polyphase fusionné, *« 16–48× fewer MACs than a 48 kHz MF »*,
`streaming_dsp.rs:161`, O(1) par échantillon) ⇒ plusieurs fois le temps réel ⇒
10 s de backlog partent en quelques centaines de ms. S'y ajoute le replay du
warm ring (jusqu'à 8 s, `rx_worker.rs:1311`) consommé d'un coup.

### Précédent interne à réutiliser

`plans/rx-late-entry-and-grid-quota.md` documente la **même classe de panne**
côté legacy en 0.10.33 : une estimation Gardner aberrante propagée → markers
hors fenêtre → cascade « infernale » de plusieurs minutes, CPU saturé, lag
> 300 ms, aucune récupération ; et 0.9 décodait le même scénario sans souci.

Correctif retenu à l'époque (**late-entry recovery**) : après N ticks consécutifs
sans aucun CW en session active, prédire la position absolue du prochain
préambule via la cadence V3, drainer le buffer jusqu'à `prédit − marge`, et
remettre l'estimation de dérive à zéro pour repartir sur de l'audio non
contaminé. Paramètres validés OTA : `LATE_ENTRY_FAIL_THRESHOLD = 3` ticks
(~600 ms), `LATE_ENTRY_MARGIN_MS = 200`. Résultat : cascade de plusieurs minutes
réduite à un saut isolé de ~600 ms.

C'est exactement la forme de remédiation à transposer au turbo (décochage
progressif au lieu de destruction complète du driver).

**Mesure matérielle à retenir du même document** : la dérive réelle de la chaîne
OTA est **~-105 ppm stable**, dominée par les crystals des codecs USB des
transceivers (séparés du TCXO RF). Voir la prédiction P4 ci-dessous.

---

## Prédictions falsifiables

À confronter aux mesures de l'étape 5. Elles sont écrites **avant** la mesure.

- **P1** — sur S2 (bruit seul avant), le turbo ne paie **pas** la grille CFO :
  `band_signal_ratio < 0.4` bloque la phase 2 (`turbo_gate.rs:329`). CPU modeste,
  et l'échec d'entrée, s'il a lieu, est imputable au seul seuil 0.15.
- **P2** — sur S3 (parole avant), la phase 2 tourne **même à CFO vrai = 0** :
  énergie en bande ⇒ ratio ≥ 0.4, et le centroïde PSD d'un spectre vocal dans la
  bande modem sort du deadband de 1.5 Hz (`cfo.rs:38`). ⇒ ~37 FFT / 800 ms,
  retard croissant, puis rattrapage brutal à l'entrée. C'est le scénario
  reproducteur du symptôme 3.
- **P3** — sur S1 (propre), turbo et legacy entrent tous les deux ; l'écart
  n'apparaît qu'avec la coloration (S4) ou le bruit/parole (S2/S3).
- **P4 — la plus importante** : `PreambleSfoTracker` déroule `dphi` sur ±0.5
  symbole entre deux préambules espacés de `V3_PREAMBLE_PERIOD_S = 4 s`. Pour la
  famille sps=32 (NORMAL/HIGH/HIGH+/HIGH++, Rs = 1500 Bd), cela fait 6000
  symboles par intervalle ⇒ **plage non ambiguë ±83 ppm**, et des points fixes
  tous les 167 ppm. À la dérive réelle mesurée de **-105 ppm**, le point fixe
  atteint depuis r=0 n'est pas -105 mais **+62 ppm** — soit 167 ppm d'erreur,
  silencieusement. Prédiction : à -105 ppm le turbo converge vers une valeur
  fausse sans aucun indicateur d'erreur, là où le legacy (Gardner ouvert +
  grille ±80 en Power Mode) mesure juste. Les familles lentes sont moins
  exposées (ROBUST ±125 ppm, ULTRA ±250 ppm) : **c'est la famille rapide,
  celle utilisée sur relais, qui est la plus vulnérable.**
- **P5** — le premier superframe (4 s) de chaque burst turbo est décodé à 0 ppm
  de correction SFO (cause ②), alors que le budget de timing inter-ancre du
  64-APSK est ~3 ppm (`v3_session.rs:430`). ⇒ pertes de CW concentrées en début
  de burst, absentes côté legacy.
- **P6** — sur S8 (trou de 1 s), le turbo émet `SessionLost` et repart en
  détection à froid ; le legacy perd seulement les segments du trou.

---

## Étape 1 — Scénarios

**Invariants** : CFO = 0 pour toute cette campagne (le cas QO-100 à CFO non nul
est une campagne séparée). Profil **HIGH+** d'abord — assez dense pour être
sensible au SFO, assez rapide pour itérer. Payload et seed identiques partout.

| # | Scénario | Paramètres sim | Cause ciblée |
|---|----------|----------------|--------------|
| S1 | Burst propre, sans préfixe | `--start-delay 0` | référence |
| S2 | 20 s de bruit IF avant | `--start-delay 20` | ① sans coût CPU (P1) |
| S3 | 20 s de **parole** avant | `--prefix-wav voix.wav` | ① + saturation CPU (P2) |
| S4 | S3 + cascade 2 bonds | `--prefix-wav voix.wav --hops 2` | coloration relais réelle |
| S5 | Entrée tardive : RX démarré au milieu du burst | découpe du WAV à t = 6 s | chemin late-entry / flywheel arrière |
| S6 | Préambule d'entrée tronqué | `--truncate-head 150 / 300 / 600` (ms) | ouverture relais (④) |
| S7 | SFO | `--drift-ppm 20 / 50 / 105` | ② et **P4** |
| S8 | Trou en cours de burst | 0.5 / 1 / 2 s | ③ (P6) |
| S9 | Cas relais nominal réaliste | S4 + `--drift-ppm 105` | combiné |

S7 à -105 ppm et S8 ne sont pas optionnels : ils testent directement les deux
trous structurels ② et ③.

## Étape 1 — Métriques

Mesurées identiquement sur les deux modems, **legacy = référence à égaler ou
battre sur chaque scénario**.

| Métrique | Unité | Cause vérifiée |
|----------|-------|----------------|
| Entrée réussie | oui/non | ① |
| Latence d'entrée : début du signal → 1er marker validé | s | ④⑤ |
| ESI uniques récupérés / K | ratio | qualité utile |
| Nombre de `SessionLost` + brickwall par burst | compte | ③ |
| Duty CPU = temps mur consommé par seconde d'audio | sans dim. | symptôme 3 |
| `max lag_ms` et forme de la courbe de retard | ms | symptôme 3 |
| ppm appliqué vs ppm vrai, et instant de la 1ʳᵉ correction | ppm, s | ② et P4 |
| Répartition par étage interne | µs/étage | étape 6 |

Note sur `--repair-pct 0` : à 0 réparation, RaptorQ exige **tous** les K ESIs, donc
« fichier décodé » devient binaire et aveugle. On pilote sur **ESI uniques / K**
et sur la latence, pas sur le succès du fichier.

---

## Étape 3 — Deltas à apporter au simulateur

`study/nbfm_channel_sim.py` couvre déjà : `--if-noise`, `--audio-noise`, `--hpf`
(CTCSS), `--post-lpf`, `--tx-clip`, `--drift-ppm`, `--thermal-ppm/-shape/-period`,
`--carrier-hz`, `--carrier-drift-hz`. `--start-delay` fait déjà le préfixe de
bruit (silence TX, porteuse + bruit IF actifs) et c'est le comportement par
défaut. GNU Radio est disponible sur la machine Linux de dev (vérifié).

À ajouter :

1. `--prefix-wav FICHIER` — concaténer un fichier voix dans `audio_in` avant le
   signal modem, pour qu'il traverse la même chaîne FM (S3).
2. `--hops N` — rejouer la chaîne N fois. Un relais est une **cascade de deux
   bonds** ; le sim est mono-bond et calibré broadcast (±5 kHz, τ = 75 µs) alors
   que la réalité amateur est ±2,5 kHz / PM 6 dB/octave (cf. CLAUDE.md). Comme la
   cause ① est une perte de corrélation due à la coloration, le nombre de bonds
   est un paramètre de premier ordre.
3. `--truncate-head MS` — couper les N premiers ms du burst (S6).
4. Optionnel : enveloppe de flutter multiplicative pour S8.

---

## Étape 4.5 — Harness et instrumentation (bloquant pour l'étape 5)

`nbfm-modem rx` décode **en legacy uniquement** (`modem-cli/src/main.rs:511` →
`rx_v2::rx_v3`). Il n'existe aucun point d'entrée WAV → turbo. À faire :

1. **Harness WAV → turbo.** Précédent utilisable : `rx_worker.rs:3162` pilote
   déjà `run_turbo_worker` depuis un test via un `Receiver<Vec<f32>>`. Alimenter
   ce channel depuis un WAV fait passer par la chaîne réelle complète (gate auto,
   détection de famille, driver, brickwall).
2. **Deux modes de cadencement**, non négociable : `--realtime` (20 ms de chunk
   toutes les 20 ms de temps mur), seul mode où le backlog et la saturation
   existent ; et `--fast` (au plus vite) qui ne mesure que le résultat de
   décodage. En `--fast`, `lag_ms` n'a aucun sens.
3. **Instrumentation symétrique.** Legacy a un profileur 10 étages toujours actif
   (`PerfBreakdown` / `take_perf()`, `rx_v2.rs:96-117`, plus `MODEM_PERF=1`). Le
   turbo n'a que des `eprintln` dispersés (`V3_LOG_GATE`, `V3_LOG_ACQ`,
   `V3_LOG_SYNC`, `V3_TIMING_LOG`, `V3_DRIFT_LOG`). Il faut l'équivalent côté
   turbo : gate phase 1 / gate phase 2 / acq MF / DSP / FFE / LDPC / RaptorQ /
   replay. Sans ça, « le temps pris par les étapes internes » n'est pas
   comparable entre les deux modems.

---

## Étape 6 — Remédiation (à remplir après mesure)

Ordre pressenti, à confirmer par les chiffres. Les deux premiers sont du pur gain
CPU sans effet sur le comportement de détection :

1. Migrer l'acquisition in-session sur `best_match_cfo_grid` (le code existe).
2. Activer `TURBO_GATED` par défaut (parquer le DSP pendant la recherche).
3. Seuil de détection **relatif** / CFAR (cause ①) — supprime la phase 2 dans le
   cas relais au lieu de la rendre moins chère.
4. Écrire l'étage `run_timing_acquisition` manquant : Gardner one-shot sur la
   fenêtre d'entrée qui *seed* `timing_seed()` avant le premier décodage (cause ②
   et P4 — lève aussi l'ambiguïté ±83 ppm en donnant un point de départ absolu).
5. Décochage progressif : garder la session warm et relancer une acquisition
   in-session sans détruire le driver ni repasser par le cold-detect
   multi-géométries (cause ③) — transposition du correctif 0.10.33.
6. Borner le self-throttle à `search_len − template` et aligner
   `CFO_SCAN_INTERVAL_MULT` sur la marge réelle de fenêtre (cause ⑤).
7. Étendre le self-throttle wall-clock au chemin in-session.

**Objectifs chiffrés** : à définir à l'étape 6 sur la base des mesures. Principe
directeur : sur **chaque** scénario, turbo ≥ legacy pour l'entrée, les ESI
uniques et le nombre de décochages, à duty CPU inférieur.

---

## Contraintes d'exécution

- La machine Linux de dev est limitée thermiquement : génération et décodes **en
  série** (ou `-j 2` au maximum), pas de balayage parallèle. Pas de
  `cargo test --workspace` ici.
- Builds Rust via `~/.cargo/bin/cargo` (le `/usr/bin/cargo` système est trop
  ancien pour `raptorq 2.0.1`).
