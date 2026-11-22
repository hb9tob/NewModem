# Plan — journal détaillé du modem, soumis au collecteur à la sortie

**Date d'ouverture** : 2026-11-22
**Branche** : `feat/modem-journal` (depuis main 6fa5bc9)
**État** : lots 0, 1, 4 committés, 4b (envoi compressé, téléchargement) aussi ; lot 2 écrit, en attente de mesure sur hôte non bridé (voir « Reprise »)
**Objet** : remédiation n° 9 de `plan_acquisition_turbo_vs_legacy.md`. Avant de
reprendre l'optimisation de l'entrée, journaliser ce que fait le modem, le temps
d'audio en attente dans chaque buffer, et l'encodage (pour chiffrer son impact
sur la réception). Le journal peut être envoyé au collecteur en quittant, afin
d'analyser ce qui se passe chez les autres.

On mesure, on n'optimise pas : aucun changement de comportement DSP ni
d'ordonnancement dans ce chantier.

Ce document est mis à jour **à chaque étape franchie** (la machine peut crasher).

---

## Journal d'avancement

| Lot | Contenu | État | Date | Sortie |
|---|---|---|---|---|
| 0 | Branche, commit du lot 1 acquisition + harness | **fait** | 2026-11-22 | aucune régression mesurée sur les 2 tests en échec (voir « Lot 0 : validation ») ; validation partielle (S5, S6, S9 non rejoués) |
| 1 | Journal, compteur de file, sonde 1 Hz, CPU | **fait** | 2026-11-22 | 27/27 tests (dont blocage de 2 s vu pendant le blocage, écrivain bloqué sans effet) ; `cpu.rs` vérifié par `cargo check` Windows isolé, crate complète non vérifiée sous Windows (pas de mingw ici) |
| 2 | Branchement RX turbo + legacy, étages turbo, compteurs capture | en attente | 2026-11-22 | 63/63 tests en optimisé ; 12 rejeux, mêmes ESI et fichiers ; jeu égal avec 872ca5f sur S1 ; **critère duty / lag pas encore tenu sur S3 / S10** (hôte bridé) ; non committé |
| 3 | Encodage, TX, sonde du thread principal | à faire | — | — |
| 4 | Collecteur : correctifs, puis route journal | **fait** | 2026-11-22 | route, page `/journals`, quota, doc ; 2/2 tests, 24/24 contrôles de bout en bout sur serveur local ; **reste le push + redéploiement par Olivier** |
| 4b | Envoi compressé (client Rust), API de liste, téléchargeur | **fait** | 2026-11-22 | gzip 7,4–8,2× ; 65/65 de bout en bout, 24/24 en non-régression, serveur local ; voir « Lot 4b » |
| 5 | GUI : dialogue de sortie, reprise, Paramètres, i18n | à faire | — | `collector_client::submit_journal` est prêt et testé, **appelé par personne** tant que ce lot manque |
| 6 | Script d'analyse + validation | en cours | 2026-11-22 | `study/analyze_modem_journal.py` committé, essayé sur journaux réels et entrée hostile ; a révélé la cause ⑨ (plan d'acquisition) ; validation AVIF à faire |

---

## Décisions (Olivier, 2026-11-22)

- Journaux **publics**, comme les sondages.
- Contenu technique **+ indicatifs des stations reçues**. Ni audio, ni image, ni
  nom de fichier.
- Proposé à la sortie **s'il y a eu activité**, avec « ne plus demander » ;
  reproposé au lancement qui suit un plantage. Aucun envoi sans clic.
- Lot 1 acquisition committé sur branche, validation partielle. Pas de push.

## Constats avant chantier (lecture des sources, 2026-11-22)

À corriger ici (bloquant pour l'envoi) :

- **C1** — collecteur : pas de `DefaultBodyLimit` ⇒ axum 0.7.9 plafonne le
  multipart à 2 097 152 o ; `--max-upload-mb` est inopérant.
- **C2** — collecteur : l'indicatif sert de segment de chemin et accepte `/`.
  `X/P` ⇒ dossier imbriqué ; `/X` ⇒ chemin absolu, écriture hors de
  `reports_dir`.

À mesurer avec le journal, pas à corriger ici :

- **M1** — `compress_image` / `compress_file_zstd` sont des commandes non async,
  donc exécutées sur le thread principal Tauri.
- **M2** — ravif utilise tous les cœurs, à la priorité du worker RX.
- **M3** — `lag_ms` (mur − audio consommé) n'est pas l'occupation du buffer :
  dérive d'horloge incluse, chunks jetés non comptés, muet pendant un replay.
- **M4** — `restart_capture` jette la file en attente sans trace.

## Architecture

```
capture ─► compteur produit ─► file mpsc ─► compteur consommé ─► worker RX
                     └──── sonde 1 Hz, indépendante du worker ────┘
événements RX / TX / encodage + sonde ─► try_send ─► thread écrivain ─► JSONL segmenté
```

- `modem-worker/src/journal/` : journal, sink à liste blanche, file mesurée,
  sonde, temps CPU. Aucune dépendance Tauri.
- Segments de 5 Mo, 100 Mo conservés, flush 2 s, envoi ≤ 20 Mo gzip.
- Collecteur : `POST /api/v1/journal`, stockage `journals/`, page `/journals`.
- Analyse : `study/analyze_modem_journal.py`.

## Critères de validation

| Critère | Cible |
|---|---|
| `acq_replay` avec / sans journal (S1, S3, S10, deux modems) | mêmes ESI, duty +0,01 max, lag +20 ms max |
| Blocage injecté de 2 s | ~2000 ms de file vus **pendant** le blocage |
| Écrivain disque bloqué | aucun effet sur le worker |
| AVIF vitesse 1 et 10 pendant une réception | impact chiffré par le script |
| Collecteur local | 10 Mo acceptés, `X/P` et `/X` restent dans le stockage |
| Application | toujours fermable (JS muet, hors ligne) |

## Lot 0 : validation (2026-11-22)

Suite `modem-worker --lib`, debug, en série : 34 OK, 2 échecs, 1 ignoré. Les deux
échecs, comparés entre main 6fa5bc9 et le lot 1 (mêmes options, hors de l'arbre) :

| Test | Conditions | main | lot 1 |
|---|---|---|---|
| `v4_chunked_worker_auto_detects_high` | debug | 0/3 OK | 0/3 OK |
| idem | optimisé | 3/3 OK | 3/3 OK |
| `turbo_worker_redetects_cross_family_burst` | debug | 2/3 OK | 1/3 OK |
| idem | debug, `V3_NO_GATE_SELFTHROTTLE=1` | 1/1 OK | 1/1 OK |
| idem | optimisé, `V3_NO_GATE_SELFTHROTTLE=1` | 1/1 OK | 1/1 OK |
| idem | optimisé | 10/10 OK | 10/10 OK |

« Optimisé » = profil release, LTO off, 16 unités de codegen. Le reste de la
suite n'a pas été rejoué en optimisé.

Lecture : ces deux tests dépendent de la vitesse de l'hôte. Ne pas les juger en
debug sur cette machine. Pour le test inter-familles, la dépendance passe par
l'auto-limitation murale du gate (cause ⑤ du plan d'acquisition) : un poll lent
espace le suivant et le 2e burst peut tomber dans le trou. À reprendre avec
l'optimisation de l'entrée.

## Collecteur : constat supplémentaire (2026-11-22)

- **C3** (non corrigé sur la route sondage) : le nom de dossier ne hache que
  `metadata + report`. Deux envois dans la même seconde qui ne diffèrent que par
  le WAV tombent dans le même dossier, le second écrase le premier (observé sur
  serveur local). Sur la route journal, le gzip entrera dans le hachage.

## Lot 2 : validation (2026-11-22)

| Scénario | Modem | Journal | Fichiers | Duty | Lag max (ms) |
|---|---|---|---|---|---|
| S1 | turbo | coupé / actif | 1 / 1 | 0,374 / 0,375 | 513 / 512 |
| S1 | legacy | coupé / actif | 1 / 1 | 0,176 / 0,176 | 871 / 886 |
| S3 | turbo | coupé / actif | 1 / 1 | 0,312 / 0,313 | 1085 / 1109 |
| S3 | legacy | coupé / actif | 1 / 1 | 0,151 / 0,156 | 917 / 908 |
| S10 | turbo | coupé / actif | 3 / 3 | 0,291 / 0,404 | 1175 / 1950 |
| S10 | legacy | coupé / actif | 3 / 3 | 0,189 / 0,135 | 2840 / 1187 |

96 ESI partout, 0 décrochage. S10 varie dans les deux sens (turbo pire avec
journal, legacy deux fois meilleur) : ces chiffres mesurent l'hôte.

**Hôte bridé ce jour-là** : pendant une compilation à 2 rustc, le cœur le plus
rapide reste à 637–791 MHz (20 relevés en 20 s) pour 3,7 GHz nominal, secteur
branché, 38 °C, compteurs de throttling thermique à 0. Cause du bridage inconnue.
Les journaux concordent : CPU thread / mur = 1,0 sur les activités lentes (le
worker n'est pas préempté, le cœur est lent) et le même poll de gate passe de
6,5–7 à 13–14 ms au cours de S10 (4,75 ms le 11.11).

Conséquences :

- les durées et lags du 2026-11-22 ne se comparent pas à ceux du 11.11 ;
- les échecs en debug du lot 0 peuvent venir de ce bridage ;
- avant toute mesure de temps, relever `scaling_cur_freq` sous charge ;
- rejouer avec / sans journal et contre 872ca5f (avant journal) sur hôte sain.

### Contrôle contre le commit d'avant le journal (872ca5f)

S1, 3 tours entrelacés, même jour, même hôte, binaire actuel avec journal coupé
(relais de file et chronos d'étages actifs) :

| Modem | Duty 872ca5f | Duty actuel | Lag max 872ca5f | Lag max actuel |
|---|---|---|---|---|
| turbo | 0,389 / 0,388 / 0,393 | 0,389 / 0,397 / 0,390 | 521 / 518 / 534 | 535 / 527 / 534 |
| legacy | 0,198 / 0,193 / 0,189 | 0,192 / 0,184 / 0,192 | 1204 / 937 / 925 | 931 / 951 / 944 |

Le code du journal n'explique donc pas l'écart avec le 11.11.

## Lot 4b : envoi compressé et téléchargement (2026-11-22)

- **Format figé** : gzip niveau 6, un membre par segment. Mesuré sur 6 journaux
  réels (0,85 Mo) : 7,7×, soit environ 0,5 Mo/h. Niveau 9 : +4 % pour presque
  2× le CPU. zstd-10 : 20 % plus petit, mais une dépendance pour qui analyse.
- 100 Mo synthétiques (enregistrements réels répétés) : 12,6 Mo envoyés, rien
  d'omis, 12,7 s sur la Surface bridée. Le lot 5 doit afficher une progression.
- `store::submission` calcule le résumé sur ce qui est envoyé ;
  `collector_client::submit_journal` signe et envoie (timeout 300 s).
- `GET /api/v1/journals` : liste JSON, du plus récent au plus ancien, journaux
  complets seulement (`metadata.json` est écrit en dernier).
- `study/fetch_modem_journals.py URL` : HTTP ou HTTPS, incrémental, liste non
  fiable (noms validés, tailles bornées, redirection même hôte seulement).
- **Défaut du lot 1 trouvé et corrigé** : l'événement RX `header` avait le
  `kind` de l'en-tête du journal ; il devient `rx_header`.
- Non testé : nginx réel, production, build Windows, clic dans la GUI (lot 5).
- **C4** (non corrigé) : `setup-nginx.sh` pose `client_max_body_size 60m` alors
  que le service et la doc disent 200 Mio. Sans effet sur les journaux (20 Mio),
  bloque les sondages au-delà de 60 Mo sur une install faite par ce script.

## Reprise (après redémarrage de la machine)

Le scratchpad peut avoir disparu ; tout le code est dans l'arbre de travail
(lot 2 **non committé**, ne pas le perdre). Un export `git archive` se compile
dans son propre `CARGO_TARGET_DIR`, jamais dans `rust/target`.

1. Vérifier l'hôte, une compilation en cours : le cœur le plus rapide doit
   dépasser nettement 1 GHz.
   `cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq | sort -n | tail -1`
2. Construire :
   `cd rust && ~/.cargo/bin/cargo build --release -j 2 -p modem-worker --example acq_replay`
   et, hors de l'arbre, le même exemple depuis `git archive 872ca5f rust`.
3. S1, S3, S10, deux modems, entrelacés, 3 tours : binaire 872ca5f, binaire
   actuel, binaire actuel avec `--journal <dossier>`
   (`acq_replay --input <wav> --mode turbo|legacy --summary <json>`).
   Critère : mêmes ESI, duty +0,01 max, lag +20 ms max, sur les médianes.
4. Critère tenu : commit du lot 2. Puis lots 3, 5, fin du 6.
