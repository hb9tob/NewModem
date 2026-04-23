# Deploy newmodem-collector on Debian 13 (Trixie) — agent notes

Cible : Infomaniak VPS Lite (1 vCPU / 2 GB RAM), Debian 13, accès SSH par
clé publique. URL exposée via DuckDNS (`<sub>.duckdns.org` → A-record IPv4
du VPS). nginx en front, TLS Let's Encrypt, binaire Rust derrière.

Coexistence avec WireGuard (UDP/51820) déjà en place : aucune
interférence — on n'utilise que TCP/80 (HTTP-01 challenge + redirect) et
TCP/443 (HTTPS).

## 0. Préparation locale

Côté poste de dev (Windows) — **génère le secret HMAC partagé** :

```bash
openssl rand -hex 32 > rust/newmodem-collector/secret.txt
cp rust/newmodem-collector/secret.txt rust/modem-gui/src-tauri/secret.txt
# Les deux fichiers sont gitignored. Ne PAS commit.
```

Le contenu doit être identique des deux côtés — sinon le serveur rejette
toutes les soumissions.

## 1. Système (sur le VPS, en root)

```bash
apt update
apt install -y \
  build-essential pkg-config curl ca-certificates \
  nginx certbot python3-certbot-nginx \
  git
```

Pas besoin de `cargo` via apt : Debian 13 ships rustc 1.85+ ce qui suffit.
Vérifie :

```bash
apt install -y rustc cargo
rustc --version   # >= 1.75 attendu
```

Si trop vieux : passer par rustup (`curl https://sh.rustup.rs | sh`).

## 2. Utilisateur dédié + arborescence

```bash
useradd --system --home /var/lib/newmodem-collector --shell /usr/sbin/nologin newmodem
mkdir -p /var/lib/newmodem-collector/reports
mkdir -p /opt/newmodem-collector
chown -R newmodem:newmodem /var/lib/newmodem-collector
```

## 3. Source + build (sur le VPS)

Pousse manuellement le source via scp / rsync, OU clone si le repo est
accessible :

```bash
# Option A : clone (repo public)
cd /opt
git clone https://github.com/hb9tob/NewModem.git
cd NewModem/rust/newmodem-collector

# Option B : push depuis ton poste (dev)
#   rsync -av --exclude target rust/ root@<vps>:/opt/NewModem/rust/

# AVANT le build : poser le secret.txt à côté du Cargo.toml.
scp rust/newmodem-collector/secret.txt root@<vps>:/opt/NewModem/rust/newmodem-collector/
```

Build release sur le VPS :

```bash
cd /opt/NewModem/rust
cargo build -p newmodem-collector --release
# Premier build : ~5-8 min, ~250 MB d'occupation /tmp.
```

Installe le binaire :

```bash
install -m 0755 target/release/newmodem-collector \
    /usr/local/bin/newmodem-collector
```

## 4. Service systemd

Crée `/etc/systemd/system/newmodem-collector.service` :

```ini
[Unit]
Description=NewModem channel-sounding collector
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=newmodem
Group=newmodem
ExecStart=/usr/local/bin/newmodem-collector \
    --bind 127.0.0.1:8080 \
    --reports-dir /var/lib/newmodem-collector/reports \
    --max-upload-mb 50
Restart=on-failure
RestartSec=3

# Hardening basique.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/var/lib/newmodem-collector
ProtectHome=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now newmodem-collector
systemctl status newmodem-collector
journalctl -u newmodem-collector -e
```

Vérifie depuis le VPS :

```bash
curl http://127.0.0.1:8080/api/v1/health
# → "ok"
```

## 5. DuckDNS

Côté DuckDNS, crée un sous-domaine (par ex. `hb9tob-modem`) et pointe
l'A-record vers l'IPv4 publique du VPS. Vérifie depuis le VPS :

```bash
dig +short hb9tob-modem.duckdns.org
# Doit renvoyer l'IP publique du VPS.
```

Optionnel : cron de mise à jour DuckDNS sur le VPS si l'IP est dynamique
(non pertinent pour un VPS qui a une IP fixe — case typique Infomaniak).

## 6. nginx + TLS

Crée `/etc/nginx/sites-available/newmodem-collector` :

```nginx
server {
    listen 80;
    listen [::]:80;
    server_name hb9tob-modem.duckdns.org;

    # certbot challenge
    location /.well-known/acme-challenge/ {
        root /var/www/html;
    }

    # tout le reste : redirige HTTPS
    location / {
        return 301 https://$host$request_uri;
    }
}

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name hb9tob-modem.duckdns.org;

    # ssl_certificate / ssl_certificate_key seront ajoutés par certbot
    # à la première exécution de `certbot --nginx`.

    # 50 MB max upload côté nginx aussi (cohérent avec --max-upload-mb).
    client_max_body_size 60m;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_request_buffering off;
        proxy_read_timeout 60s;
    }
}
```

Active le vhost :

```bash
ln -s /etc/nginx/sites-available/newmodem-collector \
      /etc/nginx/sites-enabled/newmodem-collector
nginx -t && systemctl reload nginx
```

Génère le cert Let's Encrypt :

```bash
certbot --nginx -d hb9tob-modem.duckdns.org \
    --agree-tos --no-eff-email -m hb9tob@gmail.com
# → Choisis "redirect" si proposé. Le bloc 443 sera complété en place.
```

Renouvellement auto via le timer systemd `certbot.timer` (déjà actif après
install).

Vérifie :

```bash
curl https://hb9tob-modem.duckdns.org/api/v1/health
# → "ok"

curl https://hb9tob-modem.duckdns.org/
# → page HTML "NewModem — sondages canal collectés", 0 entrée.
```

## 7. Smoke test : soumission factice

Côté poste de dev, lance ce one-liner Python (radioconda) pour vérifier
le flux complet HMAC + multipart :

```bash
/c/Users/tous/radioconda/python.exe - <<'PY'
import hashlib, hmac, json, time, requests
SECRET = open('rust/newmodem-collector/secret.txt').read().strip().encode()
URL = 'https://hb9tob-modem.duckdns.org/api/v1/sondage'
ts = int(time.time())
callsign = 'HB9TOB'
metadata = json.dumps({
    "callsign": callsign,
    "locator": "JN36ld",
    "profile": "HIGH",
    "tx_model": "TYT MD-380 (smoke test)",
    "relay": "HB9F (Salève)",
    "notes": "smoke test deploy",
    "gui_version": "test-0.0",
    "timestamp": ts,
}).encode()
report = json.dumps({"chirp_h_db": [], "snr_per_bin_db": []}).encode()
body_hash = hashlib.sha256(metadata + report).digest()
sig = hmac.new(SECRET, b'%s|%d|' % (callsign.encode(), ts) + body_hash, hashlib.sha256).hexdigest()
r = requests.post(URL,
    headers={"X-Newmodem-Signature": sig, "X-Newmodem-Timestamp": str(ts)},
    files={
        "callsign": (None, callsign),
        "metadata": ("metadata.json", metadata, "application/json"),
        "report":   ("report.json", report, "application/json"),
    },
    timeout=15,
)
print(r.status_code, r.text)
PY
```

Attends `200 {"ok":true,...}`. La page d'index doit montrer 1 entrée.

## 8. Maintenance

- **Voir les logs** : `journalctl -u newmodem-collector -f`
- **Lister les sondages** : `find /var/lib/newmodem-collector/reports -mindepth 2 -maxdepth 2 -type d`
- **Supprimer un sondage** : `rm -rf /var/lib/newmodem-collector/reports/2026-04-23/HB9XYZ_142307_a3f1`
  L'index reflète immédiatement (filesystem-only, pas d'index à reconstruire).
- **MAJ du binaire** :
  ```bash
  cd /opt/NewModem && git pull
  cd rust && cargo build -p newmodem-collector --release
  install -m 0755 target/release/newmodem-collector /usr/local/bin/newmodem-collector
  systemctl restart newmodem-collector
  ```
- **Rotation du secret HMAC** : régénère les deux `secret.txt` (côté GUI
  et côté collector), rebuild les deux, redéploie. Casse les anciens
  clients pour la session.

## 9. Troubleshooting

| Symptôme | Cause / fix |
|---|---|
| `secret.txt trop court` au démarrage | Le fichier est vide ou contient le placeholder. Génère via `openssl rand -hex 32`. |
| 401 `bad signature` à la soumission | `secret.txt` côté GUI ≠ côté serveur, ou GUI rebuild manquant après changement. |
| 400 `timestamp out of window` | Horloge VPS ou client désynchronisée de plus de 5 min. `timedatectl status`. |
| `client_max_body_size` nginx | Si l'upload échoue silencieusement → augmenter à 100m si tu envoies des WAV bruts longs. |
| Cert renewal échoue | `certbot renew --dry-run` ; vérifie que port 80 est bien atteignable depuis Internet. |
