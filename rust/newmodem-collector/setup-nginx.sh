#!/usr/bin/env bash
#
# Idempotent nginx + Let's Encrypt setup for the NewModem channel-sounding
# collector. Run on the VPS as root.
#
# Usage:
#   sudo ./setup-nginx.sh [DOMAIN] [EMAIL]
#
# Defaults match the hb9tob deployment. The script writes an HTTP-only vhost
# first so `certbot --nginx` can run the http-01 challenge; certbot then
# injects the listen-443 block with the freshly-issued certificate and
# rewrites the port-80 vhost to redirect to HTTPS. Re-running the script is
# safe: it backs up the existing vhost and uses --keep-until-expiring so a
# valid certificate is never re-issued just because we ran the script again.
#
# Prerequisites:
#   - nginx + certbot already installed
#       apt install -y nginx certbot python3-certbot-nginx
#   - DNS A-record for $DOMAIN already points at this VPS's public IPv4
#   - newmodem-collector systemd unit running on 127.0.0.1:8080
#   - TCP/80 and TCP/443 reachable from the public internet (firewall open)

set -euo pipefail

DOMAIN="${1:-hb9tob.duckdns.org}"
EMAIL="${2:-hb9tob@gmail.com}"

VHOST="/etc/nginx/sites-available/newmodem-collector"
LINK="/etc/nginx/sites-enabled/newmodem-collector"

echo "==> Domain : $DOMAIN"
echo "==> Email  : $EMAIL"

if [ "$(id -u)" -ne 0 ]; then
    echo "Must run as root (use sudo)." >&2
    exit 1
fi

# Sanity check: required binaries
for bin in nginx certbot curl; do
    command -v "$bin" >/dev/null || {
        echo "$bin not installed. apt install -y nginx certbot python3-certbot-nginx curl" >&2
        exit 1
    }
done

# Sanity check: collector listening locally
if ! ss -tln | grep -q '127\.0\.0\.1:8080'; then
    echo "WARNING: nothing listening on 127.0.0.1:8080 — start newmodem-collector first:"
    echo "    systemctl start newmodem-collector"
    echo "    systemctl status newmodem-collector"
    # not fatal — nginx will still come up, but health checks at the end will fail
fi

# Backup existing vhost
if [ -f "$VHOST" ]; then
    cp -a "$VHOST" "${VHOST}.bak.$(date +%Y%m%d-%H%M%S)"
    echo "==> Backed up existing vhost"
fi

# Write the HTTP-only vhost. certbot --nginx will inject the 443 block on top
# of this one and rewrite location / in the port-80 block to a redirect.
cat > "$VHOST" <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name $DOMAIN;

    # Coherent with --max-upload-mb=50 on the collector side (slack for the
    # multipart envelope overhead).
    client_max_body_size 60m;

    # ACME http-01 challenge directory used by certbot
    location /.well-known/acme-challenge/ {
        root /var/www/html;
    }

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_request_buffering off;
        proxy_read_timeout 60s;
    }
}
EOF

mkdir -p /var/www/html

# Remove the stock default vhost if still enabled (would conflict on :80)
rm -f /etc/nginx/sites-enabled/default

ln -sf "$VHOST" "$LINK"

echo "==> nginx -t (pre-certbot, HTTP-only)"
nginx -t

# Start or reload — `reload` is a no-op if nginx is stopped, so try both.
if systemctl is-active --quiet nginx; then
    systemctl reload nginx
else
    systemctl start nginx
fi
systemctl enable nginx >/dev/null

echo "==> certbot --nginx (issues or renews cert, injects 443 block)"
certbot --nginx \
    -d "$DOMAIN" \
    --agree-tos --no-eff-email \
    -m "$EMAIL" \
    --redirect \
    --keep-until-expiring \
    --non-interactive

echo "==> nginx -t (post-certbot, with 443 block)"
nginx -t
systemctl reload nginx

echo
echo "==> Listening sockets:"
ss -tlnp | grep -E ':80 |:443 |:8080 ' || true

echo
echo "==> Local health check (collector direct):"
curl -sS http://127.0.0.1:8080/api/v1/health && echo

echo
echo "==> Public health check (via nginx + TLS):"
curl -sS "https://$DOMAIN/api/v1/health" && echo

echo
echo "Done. The collector is reachable at: https://$DOMAIN/"
