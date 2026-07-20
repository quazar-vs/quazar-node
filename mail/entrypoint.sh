#!/usr/bin/env bash
set -euo pipefail

DOMAIN="${MAIL_DOMAIN:-quazar.website}"
HOSTNAME="${MAIL_HOSTNAME:-mail.${DOMAIN}}"
POSTMASTER="${POSTMASTER:-no-reply@${DOMAIN}}"

echo "[mail] starting Postfix+Dovecot for ${HOSTNAME} / ${DOMAIN}"

echo "$HOSTNAME" > /etc/mailname
hostname "$HOSTNAME" 2>/dev/null || true

# Self-signed TLS
CERT_DIR=/etc/ssl/mail
mkdir -p "$CERT_DIR"
if [[ ! -f "$CERT_DIR/fullchain.pem" || ! -f "$CERT_DIR/privkey.pem" ]]; then
  echo "[mail] generating self-signed TLS cert..."
  openssl req -x509 -nodes -newkey rsa:2048 -days 3650 \
    -keyout "$CERT_DIR/privkey.pem" \
    -out "$CERT_DIR/fullchain.pem" \
    -subj "/CN=${HOSTNAME}"
  chmod 640 "$CERT_DIR/privkey.pem"
fi

sed -e "s/__HOSTNAME__/${HOSTNAME}/g" \
    -e "s/__DOMAIN__/${DOMAIN}/g" \
    /etc/postfix/main.cf.template > /etc/postfix/main.cf

mkdir -p /etc/postfix/maps /etc/dovecot/users /var/mail/vhosts /var/spool/postfix/private
touch /etc/postfix/maps/vmailbox /etc/dovecot/users/passwd
chown -R vmail:vmail /var/mail/vhosts
postmap /etc/postfix/maps/vmailbox 2>/dev/null || true

# Submission / SMTPS
if ! grep -q '^submission ' /etc/postfix/master.cf; then
  cat >> /etc/postfix/master.cf <<'EOF'

submission inet n       -       y       -       -       smtpd
  -o syslog_name=postfix/submission
  -o smtpd_tls_security_level=may
  -o smtpd_sasl_auth_enable=yes
  -o smtpd_sasl_type=dovecot
  -o smtpd_sasl_path=private/auth
  -o smtpd_relay_restrictions=permit_sasl_authenticated,reject
  -o milter_macro_daemon_name=ORIGINATING

smtps     inet n       -       y       -       -       smtpd
  -o syslog_name=postfix/smtps
  -o smtpd_tls_wrappermode=yes
  -o smtpd_sasl_auth_enable=yes
  -o smtpd_sasl_type=dovecot
  -o smtpd_sasl_path=private/auth
  -o smtpd_relay_restrictions=permit_sasl_authenticated,reject
  -o milter_macro_daemon_name=ORIGINATING
EOF
fi

# Dovecot SSL
SSL_CONF=/etc/dovecot/conf.d/10-ssl.conf
if [[ -f "$SSL_CONF" ]]; then
  sed -i 's/^ssl = .*/ssl = yes/' "$SSL_CONF" || true
  sed -i "s|^#*ssl_cert = .*|ssl_cert = <$CERT_DIR/fullchain.pem|" "$SSL_CONF" || true
  sed -i "s|^#*ssl_key = .*|ssl_key = <$CERT_DIR/privkey.pem|" "$SSL_CONF" || true
  grep -q "^ssl_cert = <$CERT_DIR/fullchain.pem" "$SSL_CONF" \
    || echo "ssl_cert = <$CERT_DIR/fullchain.pem" >> "$SSL_CONF"
  grep -q "^ssl_key = <$CERT_DIR/privkey.pem" "$SSL_CONF" \
    || echo "ssl_key = <$CERT_DIR/privkey.pem" >> "$SSL_CONF"
fi

# Disable Debian system-user auth; use only our passwd-file (99-quazar.conf)
AUTH_CONF=/etc/dovecot/conf.d/10-auth.conf
if [[ -f "$AUTH_CONF" ]]; then
  sed -i 's|^!include auth-system.conf.ext|#!include auth-system.conf.ext|' "$AUTH_CONF" || true
  sed -i 's|^auth_mechanisms =.*|auth_mechanisms = plain login|' "$AUTH_CONF" || true
  sed -i 's|^#*disable_plaintext_auth =.*|disable_plaintext_auth = no|' "$AUTH_CONF" || true
fi

# Seed postmaster when password provided
if [[ -n "${SEED_MAIL_PASSWORD:-}" ]]; then
  if ! grep -q "^${POSTMASTER}:" /etc/dovecot/users/passwd 2>/dev/null; then
    echo "[mail] seeding mailbox ${POSTMASTER}"
    setup email add "$POSTMASTER" "$SEED_MAIL_PASSWORD" || true
  fi
fi

mkdir -p /var/spool/postfix/private
chown postfix:postfix /var/spool/postfix/private 2>/dev/null || true

# Logging
service rsyslog start 2>/dev/null || true

echo "[mail] starting dovecot + postfix..."
doveconf -n >/dev/null
service dovecot start
postfix check
service postfix start

echo "[mail] ready on 25/143/465/587/993"
# Keep PID 1 alive
while true; do
  if ! pgrep -x dovecot >/dev/null || ! pgrep -x master >/dev/null; then
    echo "[mail] ERROR: postfix or dovecot died" >&2
    exit 1
  fi
  sleep 10
done
