#!/usr/bin/env bash
# Let's Encrypt for quazar.website via Certbot + Nginx plugin.
# Run on the VPS as root: sudo bash scripts/certbot-quazar-website.sh
set -euo pipefail

DOMAIN="quazar.website"
WWW="www.quazar.website"
EXPECTED_IP="${EXPECTED_IP:-176.98.191.38}"
CERTBOT_EMAIL="${CERTBOT_EMAIL:-}" # export before run, e.g. you@example.com

echo "==> 1. Install Certbot (Ubuntu)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y certbot python3-certbot-nginx

echo "==> 2. Preflight: Nginx and port 80"
systemctl is-active --quiet nginx || { echo "Nginx is not running"; exit 1; }
nginx -t

echo "==> 3. Preflight: public DNS (Google 8.8.8.8)"
for name in "$DOMAIN" "$WWW"; do
  ip="$(dig @8.8.8.8 +short "$name" A 2>/dev/null | head -1 || true)"
  if [[ -z "$ip" ]]; then
    echo "FAIL: $name does not resolve publicly (SERVFAIL or empty)."
    echo "Fix NS/glue at registrar first, then:"
    echo "  dig @8.8.8.8 $DOMAIN A +short   # must show $EXPECTED_IP"
    exit 1
  fi
  if [[ "$ip" != "$EXPECTED_IP" ]]; then
    echo "WARN: $name -> $ip (expected $EXPECTED_IP). Certbot may fail HTTP-01."
  else
    echo "OK: $name -> $ip"
  fi
done

if [[ -z "$CERTBOT_EMAIL" ]]; then
  echo ""
  echo "Set email for Let's Encrypt expiry notices:"
  echo "  export CERTBOT_EMAIL='your@email.com'"
  echo "Then re-run this script, or run certbot interactively (see README in comments)."
  exit 1
fi

echo "==> 4. Issue certificate (Certbot edits Nginx)"
certbot --nginx \
  -d "$DOMAIN" \
  -d "$WWW" \
  --non-interactive \
  --agree-tos \
  --email "$CERTBOT_EMAIL" \
  --redirect

echo "==> 5. Reload Nginx"
nginx -t && systemctl reload nginx

echo "==> 6. Autorenewal check"
systemctl enable --now certbot.timer 2>/dev/null || true
certbot renew --dry-run

echo "Done. Test: curl -sI https://$DOMAIN/ | head -5"
