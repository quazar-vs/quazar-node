#!/usr/bin/env bash
set -euo pipefail

# BIND9 для делегирования quazar.website на ns1/ns2.vpsdns.host (Ubuntu).
# ВАЖНО: перед ufw enable всегда открываем SSH и веб, иначе потеряете доступ.

SERVER_IP="176.98.191.38"
DOMAIN="quazar.website"
NS1="ns1.vpsdns.host"
NS2="ns2.vpsdns.host"
NS_ZONE="vpsdns.host"

NAMED_CONF_OPTIONS="/etc/bind/named.conf.options"
NAMED_CONF_LOCAL="/etc/bind/named.conf.local"
ZONE_FILE="/etc/bind/db.${DOMAIN}"
GLUE_ZONE_FILE="/etc/bind/db.${NS_ZONE}"

log() { echo "[bind9] $*"; }

require_root() {
  [[ "${EUID}" -eq 0 ]] || { echo "Run as root." >&2; exit 1; }
}

backup_file() {
  local f="$1"
  [[ -f "$f" ]] && cp -a "$f" "${f}.bak.$(date +%Y%m%d-%H%M%S)"
}

require_root

log "Installing bind9..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y bind9 bind9utils dnsutils

backup_file "$NAMED_CONF_OPTIONS"
log "Writing $NAMED_CONF_OPTIONS..."
cat > "$NAMED_CONF_OPTIONS" <<'EOF'
options {
    directory "/var/cache/bind";
    recursion no;
    allow-recursion { none; };
    allow-query { any; };
    listen-on { any; };
    listen-on-v6 { none; };
    allow-transfer { none; };
    version "0";
    auth-nxdomain no;
};
EOF

SERIAL="$(date +%Y%m%d)01"

backup_file "$NAMED_CONF_LOCAL"
log "Writing $NAMED_CONF_LOCAL..."
cat > "$NAMED_CONF_LOCAL" <<EOF
// Managed by setup-bind9-quazar.sh

zone "${DOMAIN}" {
    type master;
    file "${ZONE_FILE}";
    allow-transfer { none; };
};

zone "${NS_ZONE}" {
    type master;
    file "${GLUE_ZONE_FILE}";
    allow-transfer { none; };
};
EOF

log "Writing $ZONE_FILE..."
cat > "$ZONE_FILE" <<EOF
\$TTL 86400
@ IN SOA ${NS1}. hostmaster.${DOMAIN}. (
    ${SERIAL}
    7200
    3600
    1209600
    3600
)

@ IN NS ${NS1}.
@ IN NS ${NS2}.

@    IN A ${SERVER_IP}
www  IN A ${SERVER_IP}
EOF

log "Writing $GLUE_ZONE_FILE (glue для имён NS)..."
cat > "$GLUE_ZONE_FILE" <<EOF
\$TTL 86400
@ IN SOA ${NS1}. hostmaster.${NS_ZONE}. (
    ${SERIAL}
    7200
    3600
    1209600
    3600
)

@ IN NS ${NS1}.
@ IN NS ${NS2}.

ns1 IN A ${SERVER_IP}
ns2 IN A ${SERVER_IP}
EOF

chmod 640 "$ZONE_FILE" "$GLUE_ZONE_FILE"
chown root:bind "$ZONE_FILE" "$GLUE_ZONE_FILE" 2>/dev/null || true

log "Configuring firewall (SSH + web + DNS before enable)..."
if command -v ufw >/dev/null 2>&1; then
  ufw allow OpenSSH comment 'SSH access' >/dev/null 2>&1 || ufw allow 22/tcp >/dev/null
  ufw allow 80/tcp comment 'HTTP' >/dev/null
  ufw allow 443/tcp comment 'HTTPS' >/dev/null
  ufw allow 53/tcp comment 'DNS' >/dev/null
  ufw allow 53/udp comment 'DNS' >/dev/null
  ufw --force enable >/dev/null 2>&1 || true
  ufw status numbered | sed -n '1,25p' || true
else
  log "ufw not found; opening 22/80/443/53 via iptables"
  for rule in "22/tcp" "80/tcp" "443/tcp" "53/tcp" "53/udp"; do
    proto="${rule#*/}"
    port="${rule%%/*}"
    iptables -C INPUT -p "$proto" --dport "$port" -j ACCEPT 2>/dev/null || \
      iptables -I INPUT -p "$proto" --dport "$port" -j ACCEPT
  done
  apt-get install -y iptables-persistent >/dev/null 2>&1 || true
  netfilter-persistent save >/dev/null 2>&1 || true
fi

log "Checking configuration..."
named-checkconf
named-checkzone "$DOMAIN" "$ZONE_FILE"
named-checkzone "$NS_ZONE" "$GLUE_ZONE_FILE"

log "Enabling named (bind9.service is an alias on Ubuntu)..."
systemctl enable --now named
systemctl restart named

log "Status:"
systemctl status named --no-pager | head -15 || true

log "Tests (+norecurse = authoritative answer):"
dig @127.0.0.1 "$DOMAIN" NS +short +norecurse || true
dig @127.0.0.1 "$DOMAIN" A +short +norecurse || true
dig @127.0.0.1 ns1.vpsdns.host A +short +norecurse || true

log "Done. In Reg.ru set NS: ${NS1}, ${NS2} and glue IP ${SERVER_IP} if required."
