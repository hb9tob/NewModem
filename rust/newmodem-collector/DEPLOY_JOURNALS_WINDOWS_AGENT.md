# Déployer la route « journaux modem » du collecteur — instructions pour un agent sous Windows

À exécuter par un agent (Claude Code) sur le poste Windows 11 d'Olivier, qui a
l'accès SSH par clé au VPS du collecteur. Référence générale de l'installation :
`DEPLOY_DEBIAN13.md` (même dossier). Ce document ne couvre que la **mise à jour**
d'un collecteur déjà en service vers la version qui accepte les journaux.

## Ce qui change

Branche `feat/modem-journal` (pas encore sur `main`), commits du collecteur :

- `ca55eb1` : `--max-upload-mb` enfin appliqué ; un indicatif ne sort plus de
  son dossier.
- `794fc45` : `POST /api/v1/journal`, page publique `/journals`, quota global.
- `26fdd55` : `GET /api/v1/journals` (liste JSON) ; `metadata.json` écrit en
  dernier.

Aucune nouvelle dépendance système. Le secret HMAC ne change pas.

## Règles pour l'agent

- **Ne jamais afficher, copier dans un message ni committer `secret.txt`**
  (ni côté VPS, ni côté poste). Vérifier sa présence et sa taille seulement.
- Ne pas toucher à WireGuard, au pare-feu, aux certificats, au DNS.
- Ne pas régénérer le secret, ne pas modifier le service au-delà de ce qui est
  décrit ici.
- Toute étape qui échoue : **s'arrêter, ne pas improviser**, revenir à
  l'ancien binaire (section « Retour arrière ») si le service est cassé, et
  rendre compte à Olivier.
- L'adresse SSH du VPS n'est pas dans le dépôt : la lire dans
  `%USERPROFILE%\.ssh\config` (hôte du collecteur) ou la demander à Olivier.
  Ci-dessous : `<vps>` (connexion root, ou utilisateur avec `sudo`).
- Domaine public : `hb9tob.duckdns.org`.

Commandes à lancer depuis PowerShell (OpenSSH est livré avec Windows 11) ou
Git Bash.

## 0. Avant de commencer

```powershell
ssh <vps> "hostname; uptime; df -h / /var/lib; free -m"
curl.exe -s https://hb9tob.duckdns.org/api/v1/health          # → ok
curl.exe -s -o NUL -w "%{http_code}\n" https://hb9tob.duckdns.org/journals   # 404 tant que l'ancienne version tourne
```

Il faut au moins 1,5 Go libre sur `/` (build) et de la RAM + swap libres
(≥ 1,5 Go). Sinon, s'arrêter et le signaler.

## 1. État du dépôt sur le VPS

```powershell
ssh <vps> "cd /opt/NewModem && git status --short && git branch --show-current && git log --oneline -1"
ssh <vps> "test -s /opt/NewModem/rust/newmodem-collector/secret.txt && wc -c < /opt/NewModem/rust/newmodem-collector/secret.txt"
```

- `git status` doit être vide (hormis des fichiers non suivis). Des fichiers
  suivis modifiés : s'arrêter et demander.
- `secret.txt` doit exister et faire 64 à 66 octets. Absent : **s'arrêter**
  (un build sans lui crée un collecteur qui refuse toutes les soumissions).

## 2. Récupérer la branche

```powershell
ssh <vps> "cd /opt/NewModem && git fetch origin && git checkout feat/modem-journal && git pull --ff-only && git log --oneline -1"
```

Le dernier commit affiché doit être celui de `origin/feat/modem-journal`
(comparer avec `git ls-remote origin feat/modem-journal` côté poste).
`secret.txt` est ignoré par git : il reste en place.

## 3. Compiler (sur le VPS)

Le VPS a 1 vCPU et 2 Go. Le profil release du workspace fait une LTO complète :
compiler sur un seul job.

```powershell
ssh <vps> "cd /opt/NewModem/rust && rustc --version && cargo build -p newmodem-collector --release -j 1 2>&1 | tail -5"
```

- Durée : 5 à 15 min. Si la session SSH risque de couper, lancer dans `tmux`
  ou `nohup ... > /tmp/collector-build.log 2>&1 &` et surveiller le log.
- **Manque de mémoire** (processus tué, `signal: 9`) : relancer avec une LTO
  allégée, sans changer le code :
  `CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4 cargo build -p newmodem-collector --release -j 1`
- **rustc trop ancien** (erreur `E0658` ou `requires rustc 1.xx`) : s'arrêter et
  le signaler (rustup est la solution prévue dans `DEPLOY_DEBIAN13.md`, à faire
  avec l'accord d'Olivier).

## 4. Installer, en gardant l'ancien binaire

```powershell
ssh <vps> "cp -p /usr/local/bin/newmodem-collector /usr/local/bin/newmodem-collector.before-journals"
ssh <vps> "install -m 0755 /opt/NewModem/rust/target/release/newmodem-collector /usr/local/bin/newmodem-collector"
```

## 5. Vérifier le service et nginx

```powershell
ssh <vps> "systemctl cat newmodem-collector | grep -E 'ExecStart|max-upload|reports-dir|ReadWritePaths'"
ssh <vps> "grep -n client_max_body_size /etc/nginx/sites-available/newmodem-collector"
```

- Les journaux vont dans `/var/lib/newmodem-collector/journals` (à côté de
  `--reports-dir`), déjà couvert par `ReadWritePaths=/var/lib/newmodem-collector`.
  Si `ReadWritePaths` ou `--reports-dir` pointent ailleurs : s'arrêter et
  demander.
- `client_max_body_size` doit être **≥ 22m** (60m posé par `setup-nginx.sh`
  ou 220m par la doc : les deux conviennent). Plus petit : s'arrêter et
  demander. Ne pas modifier nginx sans accord.

## 6. Redémarrer

```powershell
ssh <vps> "systemctl restart newmodem-collector && sleep 2 && systemctl is-active newmodem-collector && journalctl -u newmodem-collector -n 20 --no-pager"
```

`active` attendu, et aucune ligne `secret.txt too short` dans le journal.

## 7. Contrôles depuis le poste

```powershell
curl.exe -s https://hb9tob.duckdns.org/api/v1/health                         # → ok
curl.exe -s -o NUL -w "%{http_code}\n" https://hb9tob.duckdns.org/journals    # → 200
curl.exe -s https://hb9tob.duckdns.org/api/v1/journals                        # → {"journals":[...]}
curl.exe -s -o NUL -w "%{http_code}\n" https://hb9tob.duckdns.org/            # → 200 (sondages, inchangé)
```

### Envoi de test d'un journal

Depuis la copie du dépôt sur le poste, avec le Python de radioconda. Le script
lit `rust/newmodem-collector/secret.txt` **du poste** (le même secret que le
VPS) sans l'afficher.

```bash
/c/Users/tous/radioconda/python.exe - <<'PY'
import gzip, hashlib, hmac, json, time, requests
SECRET = open('rust/newmodem-collector/secret.txt').read().strip().encode()
URL = 'https://hb9tob.duckdns.org/api/v1/journal'
callsign = 'HB9TOB'
ts = int(time.time())
metadata = json.dumps({"callsign": callsign, "gui_version": "deploy-smoke-test",
                       "notes": "test de déploiement, à supprimer"}).encode()
journal = gzip.compress(b'{"kind":"header","smoke_test":true}\n')
body_hash = hashlib.sha256(metadata + journal).digest()
sig = hmac.new(SECRET, b'%s|%d|' % (callsign.encode(), ts) + body_hash, hashlib.sha256).hexdigest()
r = requests.post(URL,
    headers={"X-Newmodem-Signature": sig, "X-Newmodem-Timestamp": str(ts)},
    files={"callsign": (None, callsign),
           "metadata": ("metadata.json", metadata, "application/json"),
           "journal": ("journal.jsonl.gz", journal, "application/gzip")},
    timeout=30)
print(r.status_code, r.text)
PY
```

Attendu : `200 {"ok":true,"url":...,"folder":"AAAA-MM-JJ/HB9TOB_hhmmss_xxxxxxxx"}`.

- `401 bad signature` : le `secret.txt` du poste diffère de celui compilé sur
  le VPS. Ne rien régénérer ; le signaler.
- `400 timestamp out of window` : horloge du poste ou du VPS décalée de plus de
  5 min.
- `413` : limite nginx trop basse (étape 5).

Le journal de test est **public** : le supprimer aussitôt, avec le dossier
renvoyé dans `folder` :

```powershell
ssh <vps> "rm -rf /var/lib/newmodem-collector/journals/<folder>"
curl.exe -s https://hb9tob.duckdns.org/api/v1/journals    # ne doit plus le lister
```

## 8. Retour arrière

```powershell
ssh <vps> "install -m 0755 /usr/local/bin/newmodem-collector.before-journals /usr/local/bin/newmodem-collector && systemctl restart newmodem-collector && systemctl is-active newmodem-collector"
```

Le dépôt peut rester sur `feat/modem-journal` : seul le binaire installé compte.

## 9. Compte rendu attendu

- commit déployé (`git log --oneline -1` sur le VPS) ;
- durée du build, et si la LTO allégée a été nécessaire ;
- résultats des contrôles de l'étape 7 (codes HTTP), réponse de l'envoi de
  test et confirmation de sa suppression ;
- valeur de `client_max_body_size` relevée ;
- toute anomalie, sans jamais citer le contenu du secret.

Ensuite, côté GUI : l'URL du collecteur dans Paramètres doit être
`https://hb9tob.duckdns.org` (un POST en http est redirigé par nginx et perd son
contenu).
