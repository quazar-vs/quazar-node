#!/usr/bin/env bash
# Quazar SSH tunnel + IP forwarding / NAT for mobile VPN clients.
# Safer wrapper around setup-ssh-tunnel-quazar.sh (sshd drop-in + tunnel user).
#
# Usage (as root on the VPS):
#   sudo bash scripts/setup-ssh-tunnel.sh
#
# Env (optional):
#   SSH_GATEWAY_PORTS=yes
#   SSH_KEEPALIVE_INTERVAL=60
#   SSH_KEEPALIVE_COUNT_MAX=3
#   SSH_TUNNEL_USER=quazar-tunnel
#   SSH_NAT_IFACE=   # auto-detect if empty; fallback eth0
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
QUAZAR_SSH_SETUP="${SCRIPT_DIR}/setup-ssh-tunnel-quazar.sh"

require_root() {
  if [[ "${EUID}" -ne 0 ]]; then
    echo "Run as root: sudo bash $0" >&2
    exit 1
  fi
}

detect_wan_iface() {
  local iface=""
  if [[ -n "${SSH_NAT_IFACE:-}" ]]; then
    echo "${SSH_NAT_IFACE}"
    return 0
  fi
  # Prefer the interface used for the default IPv4 route
  iface="$(ip -4 route show default 2>/dev/null | awk '{for (i=1;i<=NF;i++) if ($i=="dev") {print $(i+1); exit}}' || true)"
  if [[ -z "${iface}" ]]; then
    iface="$(ip -o link show 2>/dev/null | awk -F': ' '$2 !~ /^(lo|docker|br-|veth|virbr|tun|tap)/ {print $2; exit}' || true)"
  fi
  if [[ -z "${iface}" ]]; then
    iface="eth0"
  fi
  echo "${iface}"
}

ensure_sysctl_ip_forward() {
  local conf="/etc/sysctl.d/99-quazar-ipforward.conf"
  if [[ -f "${conf}" ]] && grep -qE '^[[:space:]]*net\.ipv4\.ip_forward[[:space:]]*=[[:space:]]*1' "${conf}"; then
    echo "sysctl drop-in already enables ip_forward: ${conf}"
  elif grep -qE '^[[:space:]]*net\.ipv4\.ip_forward[[:space:]]*=[[:space:]]*1' /etc/sysctl.conf 2>/dev/null; then
    echo "ip_forward already set in /etc/sysctl.conf"
  else
    cat > "${conf}" <<'EOF'
# Managed by Quazar setup-ssh-tunnel.sh
net.ipv4.ip_forward=1
EOF
    echo "Wrote ${conf}"
  fi
  sysctl -w net.ipv4.ip_forward=1 >/dev/null
  sysctl --system >/dev/null 2>&1 || sysctl -p >/dev/null 2>&1 || true
  echo "net.ipv4.ip_forward=$(sysctl -n net.ipv4.ip_forward)"
}

ensure_nat_masquerade() {
  local iface="$1"
  if ! command -v iptables >/dev/null 2>&1; then
    echo "iptables not found; skipping NAT rules" >&2
    return 0
  fi

  if iptables -t nat -C POSTROUTING -o "${iface}" -j MASQUERADE 2>/dev/null; then
    echo "NAT MASQUERADE already present for -o ${iface}"
  else
    iptables -t nat -A POSTROUTING -o "${iface}" -j MASQUERADE
    echo "Added NAT MASQUERADE for -o ${iface}"
  fi

  if iptables -C FORWARD -j ACCEPT 2>/dev/null; then
    echo "FORWARD ACCEPT already present"
  else
    iptables -A FORWARD -j ACCEPT
    echo "Added FORWARD ACCEPT"
  fi
}

maybe_persist_iptables() {
  if ! command -v apt-get >/dev/null 2>&1; then
    echo "Non-Debian system: persist iptables manually if needed"
    return 0
  fi
  export DEBIAN_FRONTEND=noninteractive
  if ! dpkg -s iptables-persistent >/dev/null 2>&1; then
    echo "Installing iptables-persistent (optional persistence)..."
    apt-get update -qq
    apt-get install -y -qq iptables-persistent || {
      echo "WARNING: could not install iptables-persistent" >&2
      return 0
    }
  fi
  if command -v netfilter-persistent >/dev/null 2>&1; then
    netfilter-persistent save || true
    echo "Saved iptables via netfilter-persistent"
  fi
}

require_root

WAN_IFACE="$(detect_wan_iface)"
echo "Using WAN interface: ${WAN_IFACE}"

ensure_sysctl_ip_forward
ensure_nat_masquerade "${WAN_IFACE}"
maybe_persist_iptables

if [[ -x "${QUAZAR_SSH_SETUP}" ]]; then
  echo "Configuring OpenSSH via ${QUAZAR_SSH_SETUP}"
  # shellcheck disable=SC1090
  bash "${QUAZAR_SSH_SETUP}"
elif [[ -f "${QUAZAR_SSH_SETUP}" ]]; then
  bash "${QUAZAR_SSH_SETUP}"
else
  echo "WARNING: ${QUAZAR_SSH_SETUP} not found; only IP forward/NAT applied" >&2
fi

echo "✅ SSH-туннель настроен (iface=${WAN_IFACE})"
