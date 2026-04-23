# NBFM Modem GUI — guide utilisateur

Interface Tauri pour la transmission d'images sur relais NBFM radioamateur.
Couche graphique au-dessus du cœur modem (`modem-core`) et du CLI
(`modem-cli`).

Pour le build et le packaging Debian : voir **[BUILD_DEBIAN.md](BUILD_DEBIAN.md)**.
Pour la conception du modem et la caractérisation du canal : voir le
**[README racine](../../README.md)** et le [wiki](../../wiki/Home.md).

---

## Onglets

| Onglet | Rôle |
|---|---|
| **RX** | Réception continue, level-meter, constellation, progression RaptorQ, capture brute WAV |
| **TX** | Compression AVIF (drag-drop), choix du mode modem, émission + "TX more" pour blocs repair |
| **Sessions** | Sessions RaptorQ persistées 24 h (meta, packets.blob, fichier décodé le cas échéant) |
| **Canal** | Estimateur de canal — phase A livrée (cascade ATT), phases B/C planifiées |
| **Paramètres** | Indicatif, cartes son RX/TX, **PTT série** |
| **Info** | Journal des événements bas-niveau |

En cliquant sur l'onglet **Canal**, la RX et la TX en cours sont coupées
automatiquement (on évite de bidouiller le niveau pendant qu'on émet ou
qu'on reçoit).

---

## Configuration PTT (série RTS/DTR)

L'application pilote les lignes **RTS** et **DTR** d'un port COM (Windows)
ou `/dev/ttyUSB*` (Linux) pour déclencher la PTT de l'interface audio /
transceiver. Aucune donnée n'est écrite sur le port — seules les lignes
de contrôle sont basculées.

### Réglages dans l'onglet Paramètres

1. **Activer la commande PTT** — coche générale.
2. **Port** — dropdown alimenté par l'OS (`serialport::available_ports`).
   Bouton ↻ pour rafraîchir après avoir branché un câble.
3. **Utiliser RTS** et/ou **Utiliser DTR** — on peut piloter une seule
   ligne, les deux, ou aucune.
4. **Niveau en émission** (par ligne) : **Haut (+)** = ligne à +V en TX
   (convention la plus répandue sur les interfaces commerciales
   radioamateur) / **Bas (−)** = ligne à 0/−V en TX. Le niveau RX est
   toujours l'inverse.

### Cycle de vie

- **Au démarrage** : l'application tente d'ouvrir le port. Si OK, les
  lignes sont mises en polarité RX ; une LED verte "PTT prête sur COMx"
  apparaît. Si le port est inaccessible (câble débranché, port utilisé
  par un autre logiciel), un message rouge s'affiche et la PTT est
  désactivée pour la session.
- **À chaque sauvegarde des paramètres** : nouvelle tentative
  d'ouverture → permet de corriger un mauvais port sans redémarrer.
- **À l'émission** : bascule TX → **attente 200 ms** → lecture du WAV.
- **En fin de WAV (ou Stop)** : **attente 200 ms** de silence → bascule
  RX.

Les 200 ms couvrent le temps de commutation TX/RX du transceiver et la
latence audio de la carte son. Ni coupure de début de trame, ni queue
bavarde en fin.

### Dépannage

| Symptôme | Cause probable |
|---|---|
| "PTT indisponible : Access is denied (os error 5)" | Port déjà ouvert par un autre logiciel (autre digi-mode app, HamLib rigctld…) |
| "PTT indisponible : The system cannot find the file specified" | Port COM/tty absent (câble débranché, numéro incorrect) |
| PTT ne bascule pas malgré "PTT prête" | Vérifier la polarité (un transceiver peut inverser RTS via un optocoupleur) |
| La radio émet, mais les premières ms du WAV sont coupées | Augmenter le guard 200 ms n'est pas exposé en UI — éditer `PTT_GUARD_MS` dans `src-tauri/src/ptt.rs` |

---

## Onglet Canal — phase A (cascade ATT)

Outil de réglage du niveau d'émission basé sur les rapports oraux des
correspondants. Convention stricte TX ↔ RX :

- **Colonne TX (vert)** — tout ce qui affecte **ma** transmission se
  règle ici.
- **Colonne RX (orange)** — affichage uniquement, valeurs à communiquer
  aux autres stations.

### Colonne TX — atténuation appliquée à ma TX

Slider **-30 dB à 0 dB** (pas 0.5 dB) synchronisé avec un input
numérique. La valeur est persistée dans `settings.json` et appliquée au
WAV avant la carte son via `gain = 10^(att/20)`. Le gain linéaire est
affiché à côté (ex : `×0.501 (-6.0 dB)`).

Un bouton "0 dB" remet à zéro instantanément.

### Colonne TX — rapports reçus (oral)

Pense-bête pour noter, au fur et à mesure du QSO, ce que les
correspondants annoncent : **indicatif + valeur en dB** (Enter pour
valider). La table affiche toutes les entrées, calcule **médiane** et
**moyenne**. Un clic sur **Appliquer la médiane** recopie la valeur dans
le slider d'atténuation TX et sauvegarde.

La liste n'est pas persistée entre redémarrages : un sondage = une
session. Seule la valeur appliquée survit.

### Colonne RX — atténuation recommandée

Cartouche en lecture seule. Alimenté automatiquement par le sondeur de
canal de la **phase B** (à venir). Valeur à communiquer à la station
émettrice.

### Flux typique d'une cascade en QSO

1. Station A lance une émission connue (actuellement : un TX image
   normal, phase B fournira un signal de test dédié).
2. Stations B, C, D écoutent. Chacune estime par oreille / S-meter /
   (en phase B) mesure automatique → annonce "HB9A, baisse de 5 dB".
3. Chez **A** : l'opérateur entre les rapports dans "Rapports reçus",
   clique "Appliquer la médiane", émet à nouveau.
4. Répéter jusqu'à stabilisation — typiquement 1 à 2 itérations.

En phase B, les valeurs mesurées automatiquement côté RX alimentent
directement le cartouche "atténuation recommandée" de la colonne RX de
chaque station réceptrice, pour transmission orale (ou, en phase
suivante, OTA).

---

## Roadmap onglet Canal

| Phase | Contenu | Statut |
|---|---|---|
| **A** | Cascade ATT (TX manuel + RX placeholder) | ✅ livré |
| **B** | Sondeur : chirp 100-2700 Hz + multi-tone + PN + EVM par profil modem | planifié |
| **C** | Rapport HTML + JSON + WAV brut, export ZIP ou 7Z, `mailto:` préremplis | planifié |
| **D** | Soumission au collecteur VPS (HMAC + multipart) | ✅ squelette + prompt post-capture livrés |

Phase B/C respecteront le même découpage TX/RX strict : côté TX, bouton
"Lancer sondage" ; côté RX, détection automatique, analyse, graphiques.

---

## Soumission au collecteur — Phase D

Quand un serveur collecteur est configuré (voir
[`rust/newmodem-collector/`](../newmodem-collector/) et
[DEPLOY_DEBIAN13.md](../newmodem-collector/DEPLOY_DEBIAN13.md)), le GUI
peut pousser ses captures pour analyse offline et visualisation
collective via le serveur web.

### Configuration

**Paramètres → Collecteur de sondages (Phase D)**

- **URL** : URL HTTPS du serveur, sans suffixe (ex :
  `https://hb9tob-modem.duckdns.org`). Vide = soumission désactivée.

Le secret HMAC partagé doit être identique des deux côtés. Il est lu au
build via `include_str!("../secret.txt")` (fichier gitignoré). Pour le
poser :

```bash
openssl rand -hex 32 > rust/newmodem-collector/secret.txt
cp rust/newmodem-collector/secret.txt rust/modem-gui/src-tauri/secret.txt
# Puis rebuild GUI ET serveur — tout secret différent fait rejeter le POST.
```

Si `secret.txt` est absent au build, `build.rs` génère un placeholder
`000…` qui passe la compilation mais fait échouer toute soumission
(message explicite côté GUI). Tu peux donc cloner et builder sans avoir
le vrai secret — la soumission est juste désactivée.

### Flux : prompt post-capture brute

Après chaque clic sur **⏹ arrêter capture** dans l'onglet RX :

1. Si l'URL collecteur est renseignée, un panneau bleu apparaît au-dessus
   du compteur RaptorQ avec :
   - durée de la capture, taille estimée, chemin du WAV ;
   - champ libre **Notes** (ex : "relais HB9F, S9, QSB léger") ;
   - boutons **Soumettre au collecteur** / **Ignorer**.
2. Sur **Soumettre** : POST multipart vers `<URL>/api/v1/sondage` avec
   le WAV, l'event log JSON (toutes les lignes de l'onglet Info) et un
   `metadata.json` (callsign, profil détecté, gui_version, timestamp,
   notes). Headers HMAC : `X-Newmodem-Signature` (HMAC-SHA256 de
   `callsign | timestamp | sha256(metadata || report)`) et
   `X-Newmodem-Timestamp`.
3. Réponse OK → panneau passe en vert avec lien cliquable
   `<URL>/sondage/<date>/<folder>` qui ouvre le détail dans le
   navigateur.
4. Erreur → panneau rouge, message d'erreur, bouton Soumettre réactivé.

Si l'URL est vide, aucun prompt n'apparaît et le bouton ⏹ se comporte
comme avant (juste arrêt + finalisation du WAV local).

### Données stockées côté serveur

```
/var/lib/newmodem-collector/reports/<YYYY-MM-DD>/<callsign>_<HHMMSS>_<short_hash>/
  metadata.json   ← callsign, profil, notes, gui_version, timestamp
  report.json     ← event log sérialisé (toutes les lignes onglet Info)
  capture.wav     ← audio brut 48 kHz mono
```

Aucune base de données : l'admin gère par `rm -rf`. La page d'index web
glob ce répertoire à chaque requête.
