#!/usr/bin/env bash
# OpenSSH server settings for Quazar mobile SSH tunnels (local / remote TCP forwarding).
# Run on the VPS as root: sudo bash scripts/setup-ssh-tunnel-quazar.sh
#
# Creates a restricted system user (default: quazar-tunnel) whose authorized_keys
# file can be populated by the Quazar API (VPN_AUTHORIZED_KEYS_PATH) or manually
# from SELECT public_key FROM vpn_keys WHERE is_active.
#
# Does NOT change root authorized_keys or PermitRootLogin.
set -euo pipefail

DROP_IN="/etc/ssh/sshd_config.d/99-quazar-tunnel.conf"
TUNNEL_USER="${SSH_TUNNEL_USER:-quazar-tunnel}"
TUNNEL_HOME="${SSH_TUNNEL_HOME:-/var/lib/quazar/tunnel}"
AUTHORIZED_KEYS="${VPN_AUTHORIZED_KEYS_PATH:-${TUNNEL_HOME}/authorized_keys}"

GATEWAY_PORTS="${SSH_GATEWAY_PORTS:-yes}"
KEEPALIVE_INTERVAL="${SSH_KEEPALIVE_INTERVAL:-60}"
KEEPALIVE_COUNT_MAX="${SSH_KEEPALIVE_COUNT_MAX:-3}"

case "${GATEWAY_PORTS,,}" in
  1 | true | yes | on) GATEWAY_PORTS_VALUE="yes" ;;
  *) GATEWAY_PORTS_VALUE="no" ;;
esac

require_root() {
  [[ "${EUID}" -eq 0 ]] || { echo "Run as root." >&2; exit 1; }
}

require_root

if [[ ! -d /etc/ssh/sshd_config.d ]]; then
  mkdir -p /etc/ssh/sshd_config.d
fi

# --- restricted tunnel user (no interactive shell, home for keys only) ---
if ! id -u "$TUNNEL_USER" >/dev/null 2>&1; then
  useradd --system --create-home --home-dir "$TUNNEL_HOME" \
    --shell /usr/sbin/nologin "$TUNNEL_USER"
  echo "Created system user: $TUNNEL_USER (home=$TUNNEL_HOME)"
else
  echo "User $TUNNEL_USER already exists"
  # Ensure home exists even if user was pre-created differently
  mkdir -p "$TUNNEL_HOME"
fi

mkdir -p "$TUNNEL_HOME"
touch "$AUTHORIZED_KEYS"
chown -R "$TUNNEL_USER:$TUNNEL_USER" "$TUNNEL_HOME"
chmod 700 "$TUNNEL_HOME"
chmod 600 "$AUTHORIZED_KEYS"

# OpenSSH requires authorized_keys ownership by the user (or root) and mode 600.
# If the Quazar node container appends keys, mount this path read-write into the container
# and set VPN_AUTHORIZED_KEYS_PATH to the same path inside the container.

if [[ -f "$DROP_IN" ]]; then
  cp -a "$DROP_IN" "${DROP_IN}.bak.$(date +%Y%m%d-%H%M%S)"
fi

cat > "$DROP_IN" <<EOF
# Managed by Quazar setup-ssh-tunnel-quazar.sh — mobile client SSH tunnels
AllowTcpForwarding yes
GatewayPorts ${GATEWAY_PORTS_VALUE}
ClientAliveInterval ${KEEPALIVE_INTERVAL}
ClientAliveCountMax ${KEEPALIVE_COUNT_MAX}

# Match restricted tunnel user only — does not affect root SSH
Match User ${TUNNEL_USER}
    AllowTcpForwarding local
    X11Forwarding no
    AllowAgentForwarding no
    PermitTTY no
    ForceCommand /bin/false
    AuthorizedKeysFile ${AUTHORIZED_KEYS}
EOF

echo "Wrote $DROP_IN:"
cat "$DROP_IN"
echo
echo "Authorized keys file: $AUTHORIZED_KEYS"
echo
echo "How to install a pubkey from Quazar VPN API / DB:"
echo "  1) App calls POST /api/vpn/key (Bearer citizen key) → receives private_key once."
echo "  2) Server stores public_key in vpn_keys; if VPN_AUTHORIZED_KEYS_PATH is set,"
echo "     the node appends the pubkey to that file automatically."
echo "  3) Manual sync example:"
echo "       psql \"\$DATABASE_URL\" -Atc \\"
echo "         \"SELECT public_key FROM vpn_keys WHERE is_active AND expires_at > NOW()\" \\"
echo "         >> $AUTHORIZED_KEYS"
echo "  4) Client: ssh -N -i id_vpn -L 18443:127.0.0.1:8080 ${TUNNEL_USER}@HOST"
echo

if sshd -t 2>/dev/null; then
  echo "sshd config test OK"
else
  echo "WARNING: sshd -t failed; check syntax before restart" >&2
  exit 1
fi

if systemctl is-active --quiet ssh 2>/dev/null; then
  systemctl reload ssh
  echo "Reloaded ssh (sshd)"
elif systemctl is-active --quiet sshd 2>/dev/null; then
  systemctl reload sshd
  echo "Reloaded sshd"
else
  echo "Restart SSH manually: systemctl reload ssh"
fi

echo "Done. Set SSH_TUNNEL_ENABLED=true and VPN_KEY_TTL_HOURS=24 in Quazar .env, restart quazar-node."
echo "Optional: VPN_AUTHORIZED_KEYS_PATH=$AUTHORIZED_KEYS (mount into container if needed)."
