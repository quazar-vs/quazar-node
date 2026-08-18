#!/usr/bin/env bash
# Install shadowsocks-rust ssserver + systemd units for Quazar citizen VPN.
set -euo pipefail

SS_DIR=/var/lib/quazar/ss
BIN=/usr/local/bin/ssserver
DEPLOY=/opt/quazar/deploy/ss
PORT="${SS_PORT:-8388}"
METHOD="${SS_METHOD:-chacha20-ietf-poly1305}"

mkdir -p "$SS_DIR"
cp -f "$DEPLOY/citizen.acl" "$SS_DIR/citizen.acl"
chmod 644 "$SS_DIR/citizen.acl"

if [[ ! -x "$BIN" ]]; then
  echo "Installing shadowsocks-rust ssserver..."
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  ARCH=$(uname -m)
  case "$ARCH" in
    x86_64|amd64) ASSET_ARCH=x86_64-unknown-linux-gnu ;;
    aarch64|arm64) ASSET_ARCH=aarch64-unknown-linux-gnu ;;
    *) echo "Unsupported arch: $ARCH"; exit 1 ;;
  esac
  TAG=$(curl -fsSL https://api.github.com/repos/shadowsocks/shadowsocks-rust/releases/latest | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
  if [[ -z "$TAG" ]]; then
    TAG="v1.23.5"
  fi
  URL="https://github.com/shadowsocks/shadowsocks-rust/releases/download/${TAG}/shadowsocks-v${TAG#v}.${ASSET_ARCH}.tar.xz"
  # some releases use shadowsocks-${TAG}. instead of shadowsocks-v
  if ! curl -fsSL -o "$TMP/ss.tar.xz" "$URL"; then
    URL="https://github.com/shadowsocks/shadowsocks-rust/releases/download/${TAG}/shadowsocks-${TAG}.${ASSET_ARCH}.tar.xz"
    curl -fsSL -o "$TMP/ss.tar.xz" "$URL"
  fi
  tar -xJf "$TMP/ss.tar.xz" -C "$TMP"
  install -m 755 "$TMP/ssserver" "$BIN"
  echo "Installed $BIN ($TAG)"
fi

# Bootstrap config if missing (dummy password until node issues first credential)
if [[ ! -f "$SS_DIR/config.json" ]]; then
  PASS=$(openssl rand -hex 12)
  cat >"$SS_DIR/config.json" <<EOF
{
  "server": "0.0.0.0",
  "server_port": ${PORT},
  "method": "${METHOD}",
  "password": "${PASS}",
  "mode": "tcp_and_udp",
  "timeout": 300,
  "ipv6_first": false,
  "acl": "${SS_DIR}/citizen.acl"
}
EOF
  chmod 600 "$SS_DIR/config.json"
fi

install -m 644 "$DEPLOY/quazar-ssserver.service" /etc/systemd/system/quazar-ssserver.service
install -m 644 "$DEPLOY/quazar-ssserver-reload.path" /etc/systemd/system/quazar-ssserver-reload.path
install -m 644 "$DEPLOY/quazar-ssserver-reload.service" /etc/systemd/system/quazar-ssserver-reload.service

systemctl daemon-reload
systemctl enable --now quazar-ssserver.service
systemctl enable --now quazar-ssserver-reload.path

# Open SS port in ufw if available
if command -v ufw >/dev/null 2>&1; then
  ufw allow "${PORT}/tcp" comment 'Quazar SS citizen VPN' || true
  ufw allow "${PORT}/udp" comment 'Quazar SS citizen VPN' || true
fi

systemctl --no-pager --full status quazar-ssserver.service | head -20
echo "OK: ssserver on port ${PORT} (${METHOD}), ACL ${SS_DIR}/citizen.acl"
