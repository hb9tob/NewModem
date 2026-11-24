# Plan — acquisition turbo vs legacy : diagnostic chiffré et remédiation

**Date d'ouverture** : 2026-11-08
**Branche** : main (à l'ouverture : 4143cca, v0.20.1)
**État** : étapes 1–4.5 faites, 5 en partie ; étape 6 (optimisation) en cours
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
| 1 | Énumérer les scénarios + figer les métriques | **fait** | 2026-11-11 | sections « Scénarios » + « Métriques » |
| 2 | Générer une transmission test sans blocs RaptorQ supplémentaires | **fait** | 2026-11-11 | `results/acq_scenarios/tx_ref_HIGHPLUS.wav` |
| 3 | Adapter le simulateur de canal aux scénarios | **fait** | 2026-11-11 | `simulate_link()` + `--prefix-wav/--hops/--truncate-head/--gap/--if-chan-bw` |
| 4 | Générer les fichiers audio des scénarios | **fait** | 2026-11-11 | 16 WAV + `results/acq_scenarios/manifest.json` |
| 4.5 | Harness WAV→turbo + instrumentation symétrique | **fait** | 2026-11-11 | `examples/acq_replay.rs` + `rx_milestone` + `StampingSink` + `study/run_acq_campaign.py` |
| 5 | Décoder avec les 2 modems, mesurer étapes internes et temps | en cours | 2026-11-11 | tableau de mesures ; S10 (2e/3e fichier) ajouté |
| 6 | Plan de remédiation avec objectifs chiffrés | en cours | 2026-11-11 | section « Remédiation » — lot 1 (R0 + R8) |
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
- **P2** — *(réfutée sur S3, 2026-11-11 : gate ≈5 % CPU, le retard vient du
  replay du warm ring, cf. étape 4.5)* — sur S3 (parole avant), la phase 2 tourne **même à CFO vrai = 0** :
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

## Étapes 2–4 — Fichiers de test (faites, 2026-11-11)

### Reproduire

```bash
# Étape 2 — burst de référence (binaire release reconstruit sur 6fa5bc9)
~/.cargo/bin/cargo build --release -p modem-cli -j 2      # depuis rust/
python3 -c "import numpy as np; open('results/acq_scenarios/payload_acq.bin','wb').write(np.random.RandomState(20261108).bytes(20000))"
rust/target/release/nbfm-modem tx -i results/acq_scenarios/payload_acq.bin \
    -o results/acq_scenarios/tx_ref_HIGHPLUS.wav -p HIGH+ --callsign HB9TOB \
    --session-id A0C00001 --repair-pct 0
# Préfixe parole (pseudo-parole déterministe, aucun enregistrement réel propre dans le dépôt)
python3 study/gen_speech_like.py results/acq_scenarios/voice_like_20s.wav --duration 20 --seed 1
# Étape 4 — tous les scénarios, en série (~55 s mur)
python3 study/gen_acq_scenarios.py            # --list, --only S1_clean,...
```

Les WAV sont dans `.gitignore` (76 Mo, reproductibles) ; `manifest.json` et
`payload_acq.bin` sont suivis.

### Burst de référence

HIGH+ (32-APSK 1500 Bd β=0.20 LDPC 3/4), payload 20 000 o pseudo-aléatoire,
session `0xA0C00001`, **K = 93, n_total = 96** (`--repair-pct 0`, arrondi
segment), VOX 0,5 s, **43,1 s**, ~10 préambules (période 4 s). Décodé
bit-exact en legacy sans canal.

### Choix d'implémentation à connaître

- **Bonds** (`--hops N`) : la chaîne FM complète est rejouée N fois (HPF CTCSS,
  pré/dé-emphase, LPF, limiteur, bruit IF par bond, graine de bruit distincte
  par bond). Les bonds intermédiaires sont re-nivelés sur le RMS TX du burst
  (le relais réémet à la même excursion). La dérive d'horloge est **de bout en
  bout** et appliquée une seule fois après le dernier bond.
- **Parole** : `gen_speech_like.py` — train glottique F0 100–150 Hz + 3 formants
  normalisés, fricatives 2,5–6 kHz, enveloppe syllabique, pauses. Spectre long
  terme : 34 % < 300 Hz, 53 % 300–1000 Hz, 5,5 % 1–2,2 kHz. Nivelée au RMS du
  burst, puis 0,5 s de pause avant le VOX. Un vrai enregistrement peut
  remplacer `voice_like_20s.wav` sans autre changement (rééchantillonné à 48 kHz).
- **Troncature S6** : coupe le VOX (0,5 s) **plus** N ms du préambule —
  l'ouverture du relais dépasse la tonalité VOX et mange le 1er préambule.
  1 s de bruit avant pour donner du contexte au détecteur.
- **Trous S8** : la porteuse IF est multipliée par 0 pendant le trou, sur le
  dernier bond ⇒ le RX produit le bruit de discriminateur squelch ouvert, à
  t = 15 s après le début du burst.
- **Filtre canal IF 8 kHz** (`--if-chan-bw`, off par défaut dans le sim) :
  sans lui le discriminateur GNU Radio voit ±240 kHz de bruit et le trou sort
  à +12 dBFS (~20 dB trop chaud). Vérifié sans effet sur le chemin nominal
  (σ² = 0,0096 identique, 107/107) ; bruit de trou ≈ +5 dB au-dessus du modem.
- Clic de démarrage des filtres à t = 6 ms (crête −2 dBFS) dans tous les
  fichiers ; rien au-delà de 50 ms. Sans conséquence.
- Défaut `--drift-ppm -16` du sim **neutralisé** (0 sauf S7/S9).

### Fichiers

| Fichier | Durée | Début burst | 1er préambule | Particularité |
|---|---|---|---|---|
| S1_clean | 43,1 s | 0,0 s | 0,5 s | référence |
| S2_noise20 | 63,1 s | 20,0 s | 20,5 s | bruit IF seul avant |
| S3_voice20 | 63,6 s | 20,5 s | 21,0 s | parole avant |
| S4_voice20_2hops | 63,6 s | 20,5 s | 21,0 s | parole + 2 bonds |
| S5_late6s | 37,1 s | −6,0 s | — | fichier commence 6 s dans le burst |
| S6_trunc150 / 300 / 600 | ~43 s | 1,0 s | 1,0 s (tronqué) | préambule amputé de 150 / 300 / 600 ms |
| S7_drift−20 / −50 / −105 / +105 | 43,1 s | 0,0 s | 0,5 s ÷ (1−ppm·10⁻⁶) | SFO ; +105 = contrôle de symétrie P4 |
| S8_gap0.5s / 1s / 2s | 43,1 s | 0,0 s | 0,5 s | trou porteuse à 15 s |
| S9_repeater_drift−105 | 63,6 s | 20,498 s | 20,998 s | S4 + −105 ppm |

Valeurs exactes (corrigées de la dérive) : `results/acq_scenarios/manifest.json`.

### Contrôle de validité — legacy hors ligne (`nbfm-modem rx`, rapide)

**Pas** la mesure de l'étape 5 (pas de cadencement temps réel, pas de profil
CPU) : seulement la preuve que chaque fichier est exploitable. Avec 0 % de
réparation, « fichier » = tous les K ESIs reçus.

| Fichier | LDPC | Segments perdus | σ² | Fichier |
|---|---|---|---|---|
| S1, S2, S3, S7 (×4) | 107/107 | 5 | 0,0096 | OK |
| S4, S9 | 107/107 | 4 | 0,0101 | OK |
| S5_late6s | 85/85 | 5 | 0,0098 | KO (6 s manquantes — attendu) |
| S6_trunc150/300/600 | 96/96 | 5 | 0,0097 | KO — 1er superframe perdu |
| S8_gap0.5s | 105/107 | 5 | 0,0391 | OK |
| S8_gap1s | 103/105 | 10 | 0,0454 | KO |
| S8_gap2s | 99/101 | 31 | 0,0735 | KO |

Observations à garder pour l'étape 5 :

- Le legacy hors ligne entre **partout**, y compris parole + 2 bonds + −105 ppm :
  la référence à égaler est haute.
- S6 : le legacy perd le 1er superframe **dès 150 ms** de troncature (même
  résultat à 150/300/600) — il attend le préambule suivant. Le turbo n'a donc
  pas à faire mieux que « entrée au 2ᵉ préambule » pour égaler.
- S1 propre perd déjà 5 segments (sur 59) sans conséquence sur le fichier —
  à expliquer avant de comparer les ESI uniques (EOT / segment de queue ?).
- Les SNR de canal ne sont pas encore un facteur : si les écarts turbo/legacy
  restent nuls à 0,02, refaire S3/S4/S9 à `if_noise` plus élevé.

---

## Étape 3 — Deltas à apporter au simulateur (spécification d'origine)

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

## Étape 4.5 — Harness et instrumentation (faite, 2026-11-11)

### Harness `acq_replay`

`rust/modem-worker/examples/acq_replay.rs` — rejoue un WAV dans le **vrai**
worker via `rx_worker::spawn` (même point d'entrée que la GUI : gate auto,
détection de famille, driver, brickwall), turbo **ou** legacy.

- Chunks de 20 ms (comme cpal) libérés sur un **horaire mural absolu**
  (chunk k à t0 + (k+1)·20 ms). Tous les comportements pilotés par l'horloge
  murale — tick de scan legacy 1 s, self-throttle du gate turbo, timeouts
  `recv` — tournent comme en capture réelle, et le backlog dans le `mpsc` non
  borné peut se former. `--pace fast` existe mais ne mesure que le décodage
  (lag sans objet, et le tick mural du legacy n'y est pas représentatif).
- 8 s de bruit faible (RMS 0,01) ajoutées après le fichier pour laisser le
  worker finir le burst ; puis fermeture du channel et `join` du thread.
- CPU process (tous threads) lu dans `/proc/self/stat` toutes les 250 ms.
- Profil de départ NORMAL en auto (comme `wav_replay_diag`), Power Mode =
  défaut GUI de la plateforme.

```bash
cd rust && ~/.cargo/bin/cargo build --release -p modem-worker --example acq_replay -j 2
python3 study/run_acq_campaign.py                    # tout, en série, ~30 min
python3 study/run_acq_campaign.py --only S3_voice20 --modes turbo
python3 study/run_acq_campaign.py --table            # tableau seul
```

Sorties : `results/acq_scenarios/runs/<scénario>_<mode>.json` (résumé),
`.jsonl` (trace complète, ignorée par git), `summary.md`.

### Instrumentation ajoutée au code worker

- **`StampingSink`** (`event_sink.rs`), installé dans `rx_worker::spawn` :
  chaque événement objet émis par l'un ou l'autre worker reçoit `_audio_s`
  (position du worker dans le flux, compteur publié par les deux boucles) et
  `_wall_s`. Le harness y ajoute son propre temps mural ⇒ **retard exact au
  moment de chaque événement** : `lag = wall − _audio_s`. La GUI ignore les
  champs supplémentaires.
- **Événement `rx_milestone {stage, …}`** (`rx_worker::emit_milestone`) :
  - turbo : `gate_poll` (coût ms de chaque poll lourd + si la grille CFO a
    tourné), `gate_open` (famille, métrique, CFO, **position du préambule
    détecté**), `driver_ready` (coût du replay du warm ring), `marker` (chaque
    marker validé : **position dans le flux**, cycle, seg_id, base_esi,
    `timing_ppm` de la boucle SFO, `drift_ppm`), `drift_committed`, `rewind`,
    `session_lost`, `burst_finalised`, `eot`, `burst_exit`, `brickwall` ;
  - legacy : `scan` à chaque tick (coût probe + décodage, n_passes, drift_ppm,
    CW, convergés, σ², **position du dernier préambule dans le flux**),
    `brickwall`.
- `RxV3Worker` tient un repère `frame0_stream` (maintenu à
  `restart_session_fresh` et `rebuild_at`) pour convertir les positions de
  session en temps du fichier — instrumentation pure, n'alimente pas le DSP.
- `V3Session::timing_ppm()` : ppm appliqué par `PreambleSfoTracker` (P4).

Tests `modem-worker` (rx_worker + rx_v3_worker + event_sink) : 16/16 + 5/5 OK.

### Définition des métriques du résumé

| Champ | Définition |
|---|---|
| `entry_delay_s` | temps mural du 1er `modem_state{active}` − début vrai du signal (manifeste) |
| `entry_algo_s` | `_audio_s` à cet instant − début vrai : audio nécessaire au-delà du préambule |
| `entry.lag_ms` | retard du worker à l'entrée ; `entry_delay ≈ algo + lag` |
| `max_lag_ms` | max de `wall − _audio_s` sur tous les événements |
| `cpu_duty` / `cpu_duty_max_1s` | CPU s / audio s, et pire fenêtre de 1 s (1,0 = un cœur saturé) |
| `esi_received / esi_needed` | max des `session_progress` (K = 93) |
| `drops_in_burst` | passage actif → inactif **suivi de nouveaux ESI** (une fin propre LAST/EOT ne compte pas) |
| `gate_polls (cfo)` | polls lourds du gate turbo, dont ceux où la phase 2 CFO a tourné |
| `last_ppm` | turbo : `timing_ppm` du dernier marker ; legacy : drift du dernier scan |

### Premiers résultats (validation du harness, 2026-11-11)

| Scénario | Mode | Délai entrée | dont algo | Lag à l'entrée | Lag max | Duty | Duty max 1 s | ESI/K | Polls gate (CFO) |
|---|---|---|---|---|---|---|---|---|---|
| S1_clean | legacy | 0,56 s | 0,50 s | 57 ms | 258 ms | 0,06 | 0,27 | 96/93 | — |
| S1_clean | turbo | 0,41 s | 0,30 s | 110 ms | 110 ms | 0,26 | 0,37 | 96/93 | 50 (12) |
| S3_voice20 | legacy | 0,36 s | 0,30 s | 55 ms | 255 ms | 0,05 | 0,26 | 96/93 | — |
| S3_voice20 | turbo | **2,87 s** | 0,38 s | **2 493 ms** | 2 493 ms | 0,23 | **1,01** | 96/93 | 153 (38) |

**Le symptôme 3 est reproduit sur S3**, et sa cause n'est pas celle prédite
par P2 :

- Pendant les 20 s de parole, le gate coûte ~50 ms par seconde (≈5 % CPU), la
  phase CFO tourne ~1 poll sur 4 (aussi sur S1 propre : 12/50), et le retard
  reste < 35 ms. Le gate ouvre 0,36 s après le préambule, sans retard.
- **Tout** le retard vient de `driver_ready` : à l'ouverture, `run_turbo_worker`
  rejoue **synchroniquement les 8 s du warm ring** (`TURBO_WARM_MAX_SAMPLES`)
  dans le driver neuf — 2 456 ms de calcul. Sur S1 le ring ne contenait que
  0,78 s ⇒ 39 ms. Le replay commence 8 s avant un préambule dont la position
  est pourtant connue (`GateOpen.preamble_abs`).
- Rattrapage brutal ensuite (lag 1,46 s à t = 24 s, ~0 à t = 25 s) : la forme
  exacte du symptôme terrain.
- ⇒ **Nouvelle cause ⑧** : coût du replay du warm ring proportionnel à son
  remplissage, pas à la position du préambule. Elle frappe tout scénario où
  l'entrée est précédée de ≥ 8 s d'audio (S2, S3, S4, S9 — tous les cas
  relais réels). P2 (saturation par la grille CFO pendant la recherche) est
  **réfutée** sur S3.
- Côté legacy : le lag max de ~260 ms est le coût d'un scan (pic ponctuel,
  pas d'accumulation).

### Retards d'une minute : mécanismes mesurés (2026-11-11, i5-1035G4)

Jalons ajoutés : `rebuild` (replay du rebuild de profil exact) et
`rewind_replay` (replay de dérive), avec durée d'audio rejouée et coût.

| Essai | Résultat |
|---|---|
| S3 turbo **forcé** HIGH+ (recherche in-session sur 20 s de parole) | duty 0,32, lag < 30 ms, entrée 0,40 s |
| S2 turbo **forcé** HIGH+ (recherche in-session sur 20 s de bruit) | duty 0,32, lag < 30 ms, entrée 0,50 s |
| S9 turbo auto (parole + 2 bonds + −105 ppm) | entrée à +2,87 s (lag 2,49 s), **2/93 ESI**, `session_lost` à 25,8 s, puis 3 cycles re-détection + replay (lags 0,9 / 0,4 / 0,1 s) sans jamais redécoder |

- Le replay d'entrée est **double** : `driver_ready` 2 462 ms = replay du warm
  ring 8 s **puis** `rebuild` NORMAL→HIGH+ qui rejoue à nouveau les mêmes 8 s
  (1 241 ms). Coût ≈ 0,15 s CPU par seconde d'audio et par replay sur cet hôte.
- La recherche in-session coûte ~0,32 s CPU / s d'audio ici. Sur un hôte 3–4×
  plus lent (Pi 4), elle dépasse 1 ⇒ le retard **croît linéairement** tant
  qu'aucun marker ne valide (mode forcé, ou driver créé par une fausse
  ouverture du gate), sans aucun plafond (channel non borné) : 3 min de
  recherche à duty 1,3 ≈ 1 min de retard.
- `HISTORY_SAMPLES = 300 s` et `rebuild_at` rejoue **tout** l'historique : un
  driver auto qui a vécu longtemps avant son 1er marker (fausse ouverture)
  rejoue jusqu'à 300 s ⇒ ~45 s ici, plusieurs minutes sur Pi.
- Non reproduit sur cet hôte : une minute. À mesurer sur l'hôte terrain avec
  le même harness, et avec un scénario de recherche longue (≥ 2 min de parole).
- S9 : le turbo échoue (2/93) là où le legacy hors ligne fait 96/93 — cible
  prioritaire de l'étape 5 (P4 ? `last_ppm` = 0,0 : la boucle SFO n'a jamais
  bougé).

Remédiation candidate (à chiffrer en étape 6) : ne rejouer que depuis
`preamble_abs − marge` (la session n'a besoin que du préambule et de la suite),
ce qui ramène S3 au coût de S1.

---

## Transmissions successives : échec au 2e/3e fichier (2026-11-11)

**Observation terrain** (Ryzen 9, GUI) : les gros retards n'apparaissent jamais
au 1er fichier reçu, mais au 2e ou au 3e. L'hypothèse est une sortie de burst
mal faite, qui laisse un état résiduel pour le fichier suivant.

**Scénario S10** (`study/gen_multi_burst.py`) : 3 transmissions HIGH+ *différentes*
(payload et session_id propres), chacune précédée de parole et séparées par des
silences squelch fermé de 15 s et 40 s. La variante `--cut-tail-ms N` coupe la
fin de chaque burst : un PTT relâché avant la fin de l'audio abîme ou supprime
l'EOT. Pour lire une trace : `study/multi_burst_timeline.py <trace.jsonl>`.

**S10_multi3 (sortie propre), turbo auto** : 3/3 fichiers décodés (96/93
chacun), 0 décrochage.
- Chaque burst se termine par `burst_finalised` puis `burst_exit`, et le lag
  revient à ~10 ms entre les fichiers.
- Chaque entrée coûte les mêmes 2,5 s (replay du warm ring + rebuild, cause ⑧).
  Pas d'accumulation d'un fichier à l'autre.
- Après chaque fin de burst, le gate se **rouvre sur la queue de la
  transmission** (préambule de l'EOT). Un driver est recréé, et c'est le décodage
  de l'EOT qui le referme. **Si l'EOT est abîmé, rien ne le referme.**

**Mécanisme identifié (lecture du code)** :
- Hors `TURBO_GATED` (le défaut), un driver turbo une fois créé tourne en continu.
  Le worker ne le détruit que sur `bursts_finalised > 0`.
- `V3Session::finalize()` n'émet `SessionFinalised` que si la session était
  `Acquiring`, `Locked` ou `Finalising`.
- Un driver créé sur une fausse ouverture (queue d'EOT abîmée, parole, bruit),
  qui n'acquiert jamais, vit donc **jusqu'au fichier suivant**. Pendant ce temps :
  - il exécute la recherche in-session (≈0,32 de duty CPU ici) ;
  - son historique grossit jusqu'à `HISTORY_SAMPLES` = 300 s ;
  - s'il est resté sur le profil d'ancrage (NORMAL), le 1er marker du fichier
    suivant déclenche `rebuild_at`, qui rejoue **tout** cet historique.
    Concrètement, jusqu'à 300 s d'audio d'un coup, ce qui donne des dizaines de
    secondes de retard, puis le rattrapage brutal.
- Seul le mode gated a un délai de grâce pour les fausses ouvertures
  (`receiving_deadline`). Le chemin par défaut n'en a aucun.

**S10_multi3_cut450 (fin de chaque burst coupée de 450 ms), turbo auto** : 3/3
fichiers décodés.
- Le préambule de l'EOT valide encore un marker, puis la session décroche
  (`session_lost`, 3 markers manqués) et le driver est détruit.
- **Le driver orphelin n'est pas reproduit** : il faut une ouverture du gate
  *sans aucun marker validé*. Le cas reste plausible sur le terrain (parole,
  queue de squelch, EOT plus abîmé). Le garde-fou R8 est ajouté par prudence, et
  le journal détaillé reste nécessaire pour l'attraper sur le terrain.

### À faire pour la suite : journal détaillé soumis en un clic

Ces cas limites (mauvaise transmission, sortie pas propre) ne se reconstituent
pas après coup sans trace. Il faut donc :

1. **Des logs fins partout** sur le chemin RX, en continu et pas seulement dans
   le harness : création et destruction du driver (avec la cause), ouverture du
   gate (métrique, famille), markers, rebuild/rewind (durée, taille rejouée),
   give-up, EOT, `burst_exit`, lag worker et duty CPU périodiques, profondeur du
   canal mpsc. Les `rx_milestone` d'`acq_replay` servent de base : il faut les
   écrire aussi dans un fichier journal tournant côté application.
2. **Un journal borné** (anneau sur disque, quelques Mo, plusieurs sessions) pour
   couvrir le 2e/3e fichier sans remplir le disque.
3. **Un bouton « Envoyer le journal »** dans la GUI, qui empaquette en une
   archive le journal, la version, l'hôte (CPU), les réglages RX (profil,
   forcé/auto, désaccentuation) et, en option, la capture WAV brute des
   dernières minutes, puis la soumet en un clic. Le canal de soumission reste à
   choisir (collecteur existant ? e-mail ?).

Ce point est noté pour une session ultérieure. Il n'est pas implémenté.

---

## Étape 6 — Remédiation (à remplir après mesure)

Ordre pressenti, à confirmer par les chiffres. Les deux premiers sont du pur gain
CPU sans effet sur le comportement de détection :

0. **(nouveau, mesuré)** Replay du warm ring limité à `preamble_abs − marge`
   (cause ⑧) — supprime les ~2,5 s de retard d'entrée sur S3.
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
8. **(nouveau, S10)** Driver orphelin : détruire le driver turbo (retour au
   cold-detect) s'il n'a validé aucun marker dans un délai de grâce après sa
   création, comme le fait déjà `receiving_deadline` en mode gated. Borner aussi
   le replay de `rebuild_at` au préambule courant au lieu de tout l'historique.
9. **(nouveau)** Journal RX détaillé, persistant et soumis en un clic (voir la
   section « Transmissions successives »).

### Lot 1 (2026-11-11) — en cours

| Id | Correctif | Cible mesurable |
|---|---|---|
| R0 | Replay d'entrée limité à `preamble_abs − 1 s` (au lieu du warm ring de 8 s) ; le rebuild qui suit ne rejoue plus que cet historique court | S3 : lag à l'entrée ≤ 0,5 s (2,5 s avant) ; S1 à S9 : ESI ≥ avant |
| R8 | Driver orphelin : retour au cold-detect si aucun marker n'est validé dans `max(4 s, 3 cycles)` après la création ; milestone `driver_drop {reason}` | test unitaire ; aucune régression sur S1 à S10 |

Validation : relancer S1, S3, S5, S6_trunc300, S9, S10_multi3 en turbo et
comparer avec les mesures d'avant.

**Objectifs chiffrés** : à définir à l'étape 6 sur la base des mesures. Principe
directeur : sur **chaque** scénario, turbo ≥ legacy pour l'entrée, les ESI
uniques et le nombre de décochages, à duty CPU inférieur.

### Cause ⑨ : détecteur SC par échantillon en O(W) (mesurée le 2026-11-22)

Premier résultat du journal modem (`docs/plan_journal_modem.md`) : en turbo,
l'étage `ingest` de `process_audio_chunk_impl` est de loin le premier poste.

| Scénario | CPU process | dont `ingest` |
|---|---|---|
| S1 | 19,1 s | 13,7 s (72 %) |
| S3 | 22,4 s | 13,9 s (62 %) |
| S10 | 102,5 s | 64,3 s (63 %) |

`ScDetector::push` recalcule ses trois sommes sur toute la fenêtre à **chaque
échantillon** (boucle `0..window_samples` sur une `VecDeque`), à 48 kHz, session
verrouillée ou non, et aussi pendant chaque replay. Le temps est mesuré sur hôte
bridé : les secondes ne valent que ce jour-là, la proportion est le résultat.

Pistes, non appliquées (chantier journal = mesure seule) : sommes glissantes en
O(1) par échantillon à métrique identique, ou ne plus faire tourner le détecteur
une fois verrouillé. À traiter en premier à la reprise de l'optimisation.

---

## Contraintes d'exécution

- La machine Linux de dev est limitée thermiquement : génération et décodes **en
  série** (ou `-j 2` au maximum), pas de balayage parallèle. Pas de
  `cargo test --workspace` ici.
- Builds Rust via `~/.cargo/bin/cargo` (le `/usr/bin/cargo` système est trop
  ancien pour `raptorq 2.0.1`).
