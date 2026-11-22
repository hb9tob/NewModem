# Plan — journal détaillé du modem, soumis au collecteur à la sortie

**Date d'ouverture** : 2026-11-22
**Branche** : `feat/modem-journal` (depuis main 6fa5bc9)
**État** : lot 0 en cours
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
| 2 | Branchement RX turbo + legacy, étages turbo, compteurs capture | à faire | — | — |
| 3 | Encodage, TX, sonde du thread principal | à faire | — | — |
| 4 | Collecteur : correctifs, puis route journal | en cours | 2026-11-22 | C1 + C2 reproduits sur main puis corrigés et vérifiés sur serveur local ; à redéployer ; route journal à faire |
| 5 | GUI : dialogue de sortie, reprise, Paramètres, i18n | à faire | — | — |
| 6 | Script d'analyse + validation | à faire | — | — |

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
