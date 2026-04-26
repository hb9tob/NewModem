# Perf RX en idle — Surface Pro 7 prend du retard

**Date d'écriture** : 2026-04-26
**À exécuter par** : agent Claude Code sur la station TX Linux (même repo git, branche `main`)
**Contexte mémoire utile** : `MEMORY.md` (notamment `feedback_no_touch_working.md`,
`feedback_telecom_rigor.md`, `project_v3_refactor_done.md`).

---

## Contexte

Modem audio Rust pour radio amateur NBFM. RX en mode V3-only sliding-window.
Profils HIGH / NORMAL / MEGA / ULTRA / ROBUST, QPSK + 16-APSK, RaptorQ.
Tous les profils sauf ROBUST ont été validés OTA le 2026-04-22 (relais).

**Symptôme observé sur Surface Pro 7 (RX Windows)** : en idle (pas de
signal valide à l'antenne), le worker RX prend du retard. Les ticks
`maintenance_tick` arrivent à se chevaucher / le buffer audio s'accumule
plus vite qu'on ne le drain. Sur un PC plus puissant le problème
n'apparaît pas.

**But** : jeter plus vite les blocs sans signification (bruit pur)
**sans casser le late-entry** (acquisition marker-based quand RX
démarre mid-stream, à offset arbitraire).

---

## État actuel du hot path en idle (audit déjà fait)

Le hot path à chaque `SCAN_INTERVAL_MS` (1000 ms) :

1. `rx_worker::maintenance_tick` → `scan_and_route`
   (`rust/modem-gui/src-tauri/src/rx_worker.rs:482, 524`)
2. `rx_v2::detect_best_profile` (idle uniquement) — corrèle les 5 profils
3. `rx_v2::rx_v3_after`
   (`rust/modem-core/src/rx_v2.rs:828`)
4. `sync::find_all_preambles` — **brute-force, pas d'energy gate**
   (`rust/modem-core/src/sync.rs:73`). Buffer idle ≈ 96 k samples,
   préambule = 256 symboles. ~24 M ops/s en pire cas.
5. Pour chaque candidat : `rx_v2_single` → `marker::find_sync_in_window`
   (`rust/modem-core/src/marker.rs:215`).

**Early-rejects existants** :
- `rx_v2::probe_preamble_present` (peak/median > 50) **est appelé
  ailleurs** (gate Idle), pas en amont de `find_all_preambles`.
- Threshold 30 % du global_max dans `find_all_preambles` — élimine
  bas bruit mais s'évalue **après** avoir corrélé tout le buffer.

**Patterns recalculés à chaque appel** :
- `preamble::make_preamble()` à chaque `find_all_preambles`
- `marker::make_sync_pattern()` à chaque `find_sync_in_window`

**Pas d'instrumentation perf** : pas de timer par sous-étape, donc on
ne sait pas avec certitude **lequel** des 5 points ci-dessus domine
sur la SP7 idle.

---

## Plan en 2 phases

### Phase 1 — Instrumentation (à faire AVANT toute optimisation)

Objectif : mesurer combien de ms par tick est passé dans chacun des
points 2/4/5. Sans ça on optimise à l'aveugle.

**Patch additif** dans `rust/modem-gui/src-tauri/src/rx_worker.rs` :

- Autour de `rx_v2::detect_best_profile` (ligne ~536) : `Instant::now()`
  → durée.
- Autour de `rx_v2::rx_v3_after` (ligne ~564) : durée.
- Compteur du nombre de préambules retournés par `rx_v3_after` (déjà
  log mais loggé par décision, pas en idle).

Format de log (1 ligne par scan, déjà existant à `[scan]`) — ajouter
champs `t_detect=Xms t_rx_v3=Yms n_preambles=N`.

**Patch additif** dans `rust/modem-core/src/sync.rs` (feature-gated par
un `cfg(feature = "perf-trace")` ou un simple
`std::env::var("MODEM_PERF") == Ok("1")`) : timer autour de la boucle
de corrélation principale, log avec `eprintln!` si activé.

**Pas de changement de logique**, juste des `Instant` + log.

**Critère de sortie de phase 1** : on sait quel point domine. Je
m'attends à `find_all_preambles` (boucle sur ~33 positions ×
256 corrélations × ~96 k samples). À confirmer.

**Où capturer les chiffres** : faire tourner le RX sur la SP7 dans la
config "idle pur" (pas de signal, antenne muette / squelché),
laisser tourner 60 s, copier le log `[scan] …` produit. Le poster
dans le commit message ou dans une note `results/perf-idle-sp7-1.txt`.

---

### Phase 2 — Energy / RMS gate avant `find_all_preambles`

**À faire SEULEMENT si la phase 1 confirme que `find_all_preambles`
domine**. Si c'est `detect_best_profile` ou autre chose, revenir vers
moi avant de coder.

**Idée** : avant d'entrer dans la corrélation brute-force, calculer
le RMS du buffer (1 passe linéaire, négligeable). Si RMS sous un
seuil calibré (squelch logiciel), retourner `Vec::new()` immédiatement.

**Préserve le late-entry** : un vrai préambule porte toujours de
l'énergie ; le seuil est uniquement contre le bruit pur de fond
(canal silencieux).

**Implémentation proposée** (additive, dans `sync.rs`) :

```rust
pub fn find_all_preambles(mf: &[Complex64], _sps: usize, pitch: usize, _beta: f64) -> Vec<usize> {
    // Energy gate : skip when the buffer is essentially noise floor.
    // Calibrated against OTA captures — see results/perf-idle-sp7-*.txt.
    let rms_sqr: f64 = mf.iter().map(|c| c.norm_sqr()).sum::<f64>() / mf.len().max(1) as f64;
    if rms_sqr < ENERGY_GATE_THRESHOLD {
        return Vec::new();
    }
    // … reste inchangé
}
```

**Choix du seuil** : pas inventer. Procédure :
1. Avec la phase-1 instrumentation, ajouter aussi un log du `rms_sqr`
   du buffer à chaque scan en idle (pas de signal).
2. Lancer la SP7 idle 30 s → relever le rms_sqr typique du bruit.
3. Lancer un OTA en réception faible (un mode connu marginal,
   p. ex. ULTRA en relais) → relever rms_sqr de ces buffers.
4. Le seuil = bien en dessous du minimum OTA observé, bien au-dessus
   du bruit pur. Marge ≥ 6 dB des deux côtés.
5. Mettre la constante en haut du fichier avec un commentaire
   référencant le fichier `results/perf-idle-sp7-*.txt`.

**Si la marge n'est pas claire** (overlap idle / OTA faible), **ne
pas activer le gate** et revenir vers moi. Mieux vaut perf
moyenne qu'un mode OTA cassé silencieusement.

---

### Phase 3 (optionnelle, à discuter avant) — Cache des patterns de référence

`OnceLock<Vec<Complex64>>` pour `make_preamble()` et `make_sync_pattern()`.
Gain probable faible (trig × 32 ou × 256, négligeable face à la
corrélation), mais gratuit. **Ne pas faire en même temps que la
phase 2** pour pouvoir attribuer les gains.

---

## Contraintes dures (à NE PAS violer)

1. **Late-entry préservé** : tous les offsets de la fenêtre doivent
   rester testables (pas de sous-échantillonnage de l'espace de
   recherche). Le gate de la phase 2 saute la corrélation
   **entièrement** (binaire on/off), pas partiellement.
2. **Modules validés OTA non touchés** : `marker.rs`, `rx_v2.rs`,
   `ldpc*.rs`, `raptorq*.rs` — pas de modif de logique. Patches
   additifs seulement (instrumentation, gate en amont).
3. **Pas de feature flag mort** : si on ajoute `cfg(feature = …)` ou
   une env var pour l'instrumentation, documenter dans le commit
   comment l'activer et créer une issue/TODO pour la retirer une fois
   le seuil calibré.
4. **Tests** : la suite loopback doit rester verte
   (`cargo test --workspace`). Si la phase 2 ajoute un seuil, écrire
   un test qui passe un signal calibré au-dessus du seuil et un
   test bruit pur qui retourne vide.

---

## Validation OTA

Après phase 2, **commit avant chaque session OTA** (cf.
`feedback_commit_before_ota.md`). Capturer en réception :

- Idle 60 s → vérifier dans le log que `find_all_preambles` est
  short-circuité (gate déclenché).
- Une transmission HIGH ou NORMAL au relais → vérifier décodage à
  100 % (régression check).
- Si possible une transmission ULTRA en marginal (ce qui était le
  cas critique avant le fix DD-PLL).

Tag : `v0.1.0-ota-perf-idle-1` ou similaire.

---

## Livrable attendu

1. Branche `perf/rx-idle-instrumentation` avec phase 1.
2. Log SP7 idle 60 s posté dans `results/perf-idle-sp7-1.txt`.
3. Décision documentée : phase 2 oui/non, et si oui, le seuil
   chiffré avec sa marge.
4. Si phase 2 livrée : branche `perf/rx-idle-energy-gate` (ou
   merge sur la même branche), tag OTA, capture de validation.

Si à un point tu hésites — surtout sur le seuil énergie — pose la
question avant d'aller plus loin.
