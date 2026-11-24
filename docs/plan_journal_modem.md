# Plan — journal détaillé du modem, soumis au collecteur à la sortie

**Date d'ouverture** : 2026-11-22
**Branche** : `feat/modem-journal` (depuis main 6fa5bc9)
**État** : lots 0 à 5 committés (lot 2 : 405d009, lot 3 : c7d6f24, lot 5 : b49c1ce, le 2026-11-24, après remesure sur hôte sain) ; lot 6 (analyse et validation) en cours ; branche non poussée
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
| 2 | Branchement RX turbo + legacy, étages turbo, compteurs capture | **fait** | 2026-11-22 | 63/63 tests en optimisé ; 12 rejeux, mêmes ESI et fichiers ; jeu égal avec a02ef65 sur S1 ; 54 rejeux entrelacés : ESI et duty tenus, lag non tenu en turbo sur S3 / S10 (hôte bridé) ; **remesure sur hôte sain le 2026-11-23 : critère tenu partout** ; commit 405d009 |
| 3 | Encodage, TX, sonde du thread principal | **fait** | 2026-11-22 | worker 67/67 en optimisé, parité TX avec le CLI du 11.11, GUI réelle isolée ; commit c7d6f24 |
| 4 | Collecteur : correctifs, puis route journal | **fait** | 2026-11-22 | route, page `/journals`, quota, doc ; 2/2 tests, 24/24 contrôles de bout en bout sur serveur local ; **reste le push + redéploiement par Olivier** |
| 4b | Envoi compressé (client Rust), API de liste, téléchargeur | **fait** | 2026-11-22 | gzip 7,4–8,2× ; 65/65 de bout en bout, 24/24 en non-régression, serveur local ; voir « Lot 4b » |
| 5 | GUI : dialogue de sortie, reprise, Paramètres, i18n | **fait** | 2026-11-22 | 6/6 tests de l'automate de sortie ; 51/51 contrôles dans la vraie GUI isolée (voir « Lots 3 et 5 ») ; commit b49c1ce |
| 6 | Script d'analyse + validation | en cours | 2026-11-22 | `study/analyze_modem_journal.py` committé, essayé sur journaux réels et entrée hostile ; a révélé la cause ⑨ (plan d'acquisition) ; AVIF vitesse 1 et 10 pendant une réception : fait, voir « Lots 3 et 5 » |

---

## Décisions (Olivier, 2026-11-22)

- Journaux **publics**, comme les sondages.
- Contenu technique **+ indicatifs des stations reçues**. Ni audio, ni image, ni
  nom de fichier.
- Proposé à la sortie **s'il y a eu activité**, avec « ne plus demander » ;
  reproposé au lancement qui suit un plantage. Aucun envoi sans clic.
- Lot 1 acquisition committé sur branche, validation partielle.
- 2026-11-22, après le lot 4b : commits signés (DCO) et branche poussée à sa
  demande (les identifiants de commit ont changé, mêmes contenus), puis lots 3
  et 5 lancés.

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
- rejouer avec / sans journal et contre a02ef65 (avant journal) sur hôte sain.

### Contrôle contre le commit d'avant le journal (a02ef65)

S1, 3 tours entrelacés, même jour, même hôte, binaire actuel avec journal coupé
(relais de file et chronos d'étages actifs) :

| Modem | Duty a02ef65 | Duty actuel | Lag max a02ef65 | Lag max actuel |
|---|---|---|---|---|
| turbo | 0,389 / 0,388 / 0,393 | 0,389 / 0,397 / 0,390 | 521 / 518 / 534 | 535 / 527 / 534 |
| legacy | 0,198 / 0,193 / 0,189 | 0,192 / 0,184 / 0,192 | 1204 / 937 / 925 | 931 / 951 / 944 |

Le code du journal n'explique donc pas l'écart avec le 11.11.

### Remesure entrelacée (2026-11-22 soir) : critère de lag NON tenu

54 rejeux (3 tours, 3 scénarios, 2 modems, 3 variantes), médianes, cœur le plus
rapide à 611–1005 MHz. 96 ESI dans les 54 rejeux.

| Scénario | Modem | Duty a02ef65 / coupé / actif | Lag max a02ef65 / coupé / actif (ms) | Critère |
|---|---|---|---|---|
| S1 | turbo | 0,375 / 0,379 / 0,381 | 505 / 512 / 511 | tenu |
| S1 | legacy | 0,183 / 0,177 / 0,178 | 899 / 899 / 901 | tenu |
| S3 | turbo | 0,315 / 0,319 / 0,321 | 948 / 996 / 1049 | **lag +48 / +101** |
| S3 | legacy | 0,151 / 0,157 / 0,156 | 935 / 931 / 926 | tenu |
| S10 | turbo | 0,273 / 0,277 / 0,280 | 1055 / 1140 / 1094 | **lag +85 / +39** |
| S10 | legacy | 0,147 / 0,127 / 0,137 | 2005 / 979 / 1900 | tenu (très dispersé) |

En turbo le maximum est, 24 fois sur 27, le rejeu unique de l'entrée en burst.
La référence seule varie de 933 à 1143 ms sur S3.

Contrôle ajouté après l'échec, qui ne remplace pas le critère : instructions en
espace utilisateur (perf_event_open), turbo, `--pace fast`, 3 tours.

| Scénario | a02ef65 | journal coupé | journal actif |
|---|---|---|---|
| S1 | 66,9947 G | 66,9997 G (+0,008 %) | 67,0119 G (+0,026 %) |
| S3 (polls du gate : 145–146 / 147 / 147) | 74,3963 G | 74,4182 G (+0,029 %) | 74,4337 G (+0,050 %) |

- Le calcul ajouté est trop petit pour expliquer +40 à +100 ms sur un rejeu
  d'environ 1 s (5 millions d'instructions sur tout S1, journal coupé).
- **Cela ne blanchit pas l'ordonnancement** : `--pace fast` ne reproduit pas les
  réveils du temps réel. Le relais de file tourne aussi journal coupé, et le
  duty turbo monte de 0,004 à 0,007 sur les trois scénarios (seuil 0,01).
- À rejouer sur hôte sain, sans changer le critère.

Les journaux des rejeux « actif » montrent le même point : sur les 9 rejeux
turbo, les activités les plus lentes (push, replay_rebuild, replay_warm) ont
CPU / mur = 0,999–1,000. Le worker n'est pas préempté au pic : il calcule sur
un cœur lent. Inférence, pas preuve : l'écart vient de la fréquence du moment.

## Lots 3 et 5 : vérification (2026-11-22 soir)

- `cargo check` GUI : propre (1 avertissement du lot 3 corrigé).
- `modem-worker --lib` en optimisé (LTO off, 16 unités) : 67 OK, 1 ignoré,
  dont les deux tests sensibles à l'hôte du lot 0.
- `cli_parity` : échantillons TX identiques au binaire CLI du 2026-11-11,
  construit avant ce chantier (un profil, NORMAL).
- GUI : 6/6 tests de l'automate de sortie.

**Vraie GUI, construite depuis un export de l'arbre de travail** (secret jetable,
crochet de fermeture externe pour test seulement, pilote JS qui clique les vrais
boutons), dans un espace de noms sans réseau extérieur, sans carte son ni port
série, HOME jetable, compositeur Wayland virtuel, collecteur local :
**51/51 contrôles**, après correction du seuil d'un contrôle du banc (« JS bloqué » attendait 25 s alors que le crochet ferme à 45 s ; la GUI sortait bien 3 s après).

| Scénario | Résultat |
|---|---|
| sans activité | sortie directe, journal clos |
| envoyer et quitter | 1 journal au collecteur, remarque et version, aucun chemin / URL / nom de fichier / périphérique dans le gzip |
| quitter sans envoyer, annuler, 2e fermeture | rien d'envoyé ; annuler laisse ouvert et redemande ; 2e fermeture sort en 0,3 s |
| ne plus demander | réglage enregistré, autres réglages intacts, sortie directe ensuite |
| sans URL, sans indicatif | sortie directe ; bouton Envoyer grisé avec explication |
| hors ligne | échec affiché, journal gardé, renvoyé ensuite depuis Paramètres |
| JS bloqué (boucle infinie) | sortie seule 3,0 s après la demande de fermeture |
| `kill -9` | session proposée au lancement suivant malgré « ne plus demander », « Plus tard » la garde, envoi marqué plantage, plus reproposée ; refus définitif |
| envoi manuel de la session en cours | envoyé, le dialogue de sortie revient quand même |
| kiosque 800×480, en anglais et en français | le dialogue tient dans l'écran |
| TX vers un périphérique absent | étages `fec`, `modulation`, `post_process` chronométrés, erreur journalisée sans texte |

**RX + AVIF vitesse 1 puis 10 (S1 lu par « fichier WAV », legacy auto)** : fichier
décodé (HIGH+), journal de 56 Kio, 7,4 Kio compressé.

- AVIF vitesse 1, 1024×768 → 800×600 : 58,6 s, dont 58,0 s d'encodage ;
  vitesse 10 : 1,4 s.
- **Thread principal de la GUI bloqué 57,7 s** pendant l'AVIF vitesse 1 : M1
  confirmé et chiffré (fenêtre, fermeture, `restart_capture` en attente).
- Processus à 1,24 cœur pendant l'encodage, 0,92 sans : sur cet hôte bridé,
  ravif n'a pas pris tous les cœurs (M2 non observé ici).
- File RX : max 500 ms pendant, 1000 ms sans. **Peu probant** : la lecture WAV
  livre des blocs de 500 ms, donc la file se lit au pas de 500 ms. Il faut une
  carte son pour chiffrer l'impact au niveau de la file.

**Changements de comportement apportés par le lot 5** (à signaler) :

- un TX arrêté pendant l'encodage se termine sans passer en émission ;
- pendant la fermeture, RX, TX et PTT ne redémarrent plus, et le port PTT reste
  ouvert jusqu'à la fin du processus (le fermer passerait en émission une
  radio câblée « bas = TX »).

**C5** (existant, non corrigé) : `settings::load` remet l'URL par défaut quand
l'URL du collecteur est vide, alors que Paramètres dit « vide = aucun envoi ».
Le lot 5 lit le fichier brut pour respecter l'URL vide.

**Non testé** : vraie carte son, SDR, PTT et radio ; build Windows ; nginx et
production ; clic par un humain ; rendu d'une GUI dont la locale système est
française (la machine n'a pas `fr_FR` ; français forcé par le pilote).

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
   et, hors de l'arbre, le même exemple depuis `git archive a02ef65 rust`.
3. S1, S3, S10, deux modems, entrelacés, 3 tours : binaire a02ef65, binaire
   actuel, binaire actuel avec `--journal <dossier>`
   (`acq_replay --input <wav> --mode turbo|legacy --summary <json>`).
   Critère : mêmes ESI, duty +0,01 max, lag +20 ms max, sur les médianes.
4. Critère tenu : commit du lot 2, puis des lots 3 et 5, qui en dépendent.
   Critère non tenu : décision d'Olivier avant tout commit de ces trois lots.
   **Décision d'Olivier (2026-11-23)** : attendre une Surface saine. Rien n'est
   committé ; après redémarrage, rejouer la mesure entrelacée (environ 2 h),
   puis commits signés des lots 2, 3 et 5 si le critère passe.

### Remesure sur Surface saine (2026-11-23) : critère TENU

- Hôte après redémarrage : cœur le plus rapide à 3,6 GHz sous charge, secteur,
  32–52 °C, compteurs de throttle à 0 ; encore 3,4 GHz sous charge après la
  série.
- Binaires release : arbre de travail et `git archive a02ef65 rust` (target séparé).
- 54 rejeux temps réel entrelacés (3 tours × S1/S3/S10 × turbo/legacy ×
  a02ef65/coupé/actif), 22 h 02 – 23 h 54, résultats et script dans
  `results/acq_scenarios/runs/remeasure_2026-11-23/` (un `.freq` par rejeu :
  fréquence max relevée toutes les 2 s ; médiane 1,1 GHz car le gouverneur
  `powersave` redescend entre les blocs, p95 3,0 GHz, max 3,7 GHz).
- 96 ESI et fichier complet dans les 54 rejeux, aucune sortie d'erreur.

| Scénario | Modem | Duty a02ef65 / coupé / actif | Lag max a02ef65 / coupé / actif (ms) | Écart lag | Critère |
|---|---|---|---|---|---|
| S1 | turbo | 0,249 / 0,253 / 0,253 | 110 / 110 / 110 | +0 / +0 | tenu |
| S1 | legacy | 0,057 / 0,058 / 0,062 | 239 / 241 / 252 | +3 / +13 | tenu |
| S3 | turbo | 0,196 / 0,197 / 0,199 | 221 / 224 / 226 | +3 / +5 | tenu |
| S3 | legacy | 0,051 / 0,052 / 0,053 | 247 / 249 / 249 | +2 / +2 | tenu |
| S10 | turbo | 0,172 / 0,174 / 0,176 | 229 / 233 / 233 | +4 / +4 | tenu |
| S10 | legacy | 0,045 / 0,048 / 0,047 | 260 / 278 / 264 | +18 / +4 | tenu (de justesse, coupé : 261 / 278 / 303) |

- Lags 4 à 8 fois plus courts que le 2026-11-22 et dispersion de quelques ms :
  l'échec de la veille venait bien du bridage de l'hôte.
- Duty : +0,001 à +0,005 (seuil 0,01).
- Commits signés (DCO) le 2026-11-24, accord d'Olivier : lot 2 `405d009`,
  lot 3 `c7d6f24`, lot 5 `b49c1ce`. Chaque état intermédiaire compile (worker,
  exemples, GUI) ; tests du journal 29 → 29 (+ TX) → 31, automate de sortie 6/6. Le
  court-circuit « TX arrêté pendant l'encodage » est rangé dans le lot 5.
