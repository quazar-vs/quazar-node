#!/usr/bin/env bash
# Install a Quazar mayor node: fetch setup package from the network, write .env, start Docker.
#
# Network base URL (pick one):
#   --network-url URL
#   QUAZAR_NETWORK_URL   (default: https://quazar.website — production registry)
#   For local dev against a registry on this machine: http://localhost:8080
#
# API key (Founder/Aiya/citizen key that owns the pending mayor application for --ip):
#   --api-key KEY | QUAZAR_API_KEY | interactive prompt
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_NETWORK_URL="${QUAZAR_NETWORK_URL:-https://quazar.website}"

NODE_IP=""
API_KEY="${QUAZAR_API_KEY:-}"
NETWORK_URL="$DEFAULT_NETWORK_URL"
INSTALL_DIR="$ROOT"
FORCE=0

usage() {
  cat <<'EOF'
Usage: install-node.sh --ip PUBLIC_IP [options]

Required:
  --ip ADDR              Public IP (or host) registered in the mayor node application

Options:
  --api-key KEY          Citizen API key (or set QUAZAR_API_KEY)
  --network-url URL      Registry base URL (or QUAZAR_NETWORK_URL; default https://quazar.website)
  --dir PATH             Directory with docker-compose.yml (default: repo root)
  --force                Overwrite existing .env without prompting
  -h, --help             Show this help

Example:
  ./scripts/install-node.sh --ip 203.0.113.10 --api-key qz_api_...
EOF
}

die() {
  echo "install-node: $*" >&2
  exit 1
}

require_cmd() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || die "required command not found: $cmd (install it and retry)"
}

strip_0x() {
  local s="$1"
  case "$s" in
    0x* | 0X*) echo "${s#??}" ;;
    *) echo "$s" ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ip)
      [[ $# -ge 2 ]] || die "--ip requires a value"
      NODE_IP="$2"
      shift 2
      ;;
    --api-key)
      [[ $# -ge 2 ]] || die "--api-key requires a value"
      API_KEY="$2"
      shift 2
      ;;
    --network-url)
      [[ $# -ge 2 ]] || die "--network-url requires a value"
      NETWORK_URL="$2"
      shift 2
      ;;
    --dir)
      [[ $# -ge 2 ]] || die "--dir requires a value"
      INSTALL_DIR="$2"
      shift 2
      ;;
    --force)
      FORCE=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1 (try --help)"
      ;;
  esac
done

[[ -n "$NODE_IP" ]] || die "--ip is required (see --help)"

require_cmd curl
require_cmd jq
require_cmd docker

if [[ -z "$API_KEY" ]]; then
  if [[ -t 0 ]]; then
    read -r -s -p "QUAZAR API key: " API_KEY
    echo
  fi
fi
[[ -n "$API_KEY" ]] || die "API key missing: pass --api-key, set QUAZAR_API_KEY, or run interactively"

INSTALL_DIR="$(cd "$INSTALL_DIR" && pwd)"
[[ -f "$INSTALL_DIR/docker-compose.yml" ]] || die "docker-compose.yml not found in $INSTALL_DIR"

ENV_FILE="$INSTALL_DIR/.env"
if [[ -f "$ENV_FILE" && "$FORCE" -ne 1 ]]; then
  die ".env already exists at $ENV_FILE (use --force to overwrite)"
fi

NETWORK_URL="${NETWORK_URL%/}"
SETUP_URL="${NETWORK_URL}/api/mayor/setup-package"

echo "Fetching setup package from ${SETUP_URL} for ip=${NODE_IP} ..."

HTTP_BODY=""
HTTP_CODE=""
if ! HTTP_BODY="$(curl -sS -G \
  "$SETUP_URL" \
  --data-urlencode "ip=${NODE_IP}" \
  -H "Authorization: Bearer ${API_KEY}" \
  -H "Accept: application/json" \
  -w $'\n__HTTP_CODE__%{http_code}')" ; then
  die "HTTP request failed (check network URL and connectivity)"
fi

HTTP_CODE="${HTTP_BODY##*$'\n__HTTP_CODE__'}"
HTTP_BODY="${HTTP_BODY%$'\n__HTTP_CODE__'*}"

if [[ "$HTTP_CODE" != "200" ]]; then
  err_msg="$(echo "$HTTP_BODY" | jq -r '.error // empty' 2>/dev/null || true)"
  if [[ -n "$err_msg" ]]; then
    die "setup-package HTTP ${HTTP_CODE}: ${err_msg}"
  fi
  die "setup-package HTTP ${HTTP_CODE}: ${HTTP_BODY}"
fi

API_STATUS="$(echo "$HTTP_BODY" | jq -r '.status // empty')"
if [[ "$API_STATUS" != "success" ]]; then
  err_msg="$(echo "$HTTP_BODY" | jq -r '.error // "unknown error"')"
  die "setup-package rejected: ${err_msg}"
fi

PKG="$HTTP_BODY"
node_id="$(echo "$PKG" | jq -r '.data.node_id // empty')"
node_secret="$(echo "$PKG" | jq -r '.data.node_secret // empty')"
reg_secret="$(echo "$PKG" | jq -r '.data.reg_secret // empty')"
peer_url="$(echo "$PKG" | jq -r '.data.peer_url // empty')"
sync_peer="$(echo "$PKG" | jq -r '.data.sync_peer // empty')"
pkg_api_key="$(echo "$PKG" | jq -r '.data.api_key // empty')"
network_id="$(echo "$PKG" | jq -r '.data.network_id // empty')"
pkg_version="$(echo "$PKG" | jq -r '.data.version // empty')"
citizen_name="$(echo "$PKG" | jq -r '.data.citizen_name // empty')"
qz_id="$(echo "$PKG" | jq -r '.data.qz_id // empty')"

[[ -n "$node_id" ]] || die "package missing node_id"
[[ -n "$node_secret" ]] || die "package missing node_secret"
[[ -n "$reg_secret" ]] || die "package missing reg_secret"

node_secret="$(strip_0x "$node_secret")"
reg_secret="$(strip_0x "$reg_secret")"

admin_key="$API_KEY"
if [[ -n "$pkg_api_key" ]]; then
  admin_key="$pkg_api_key"
fi

# QUAZAR_BOOTSTRAP_PEERS expects id@url (see README / main.rs bootstrap parser).
bootstrap_peers=""
if [[ -n "$sync_peer" ]]; then
  bootstrap_peers="$sync_peer"
elif [[ "$peer_url" == *"@"* ]]; then
  bootstrap_peers="$peer_url"
elif [[ -n "$peer_url" ]]; then
  echo "install-node: warning: peer_url is bare URL without node id; set QUAZAR_BOOTSTRAP_PEERS manually if sync fails" >&2
  bootstrap_peers=""
fi

postgres_password="$(openssl rand -base64 32 | tr -d '\n' | tr '/+' '_-' | head -c 32)"
node_url="http://${NODE_IP}:8080"

echo "Writing ${ENV_FILE} (node_id=${node_id}, citizen=${citizen_name:-?}, qz_id=${qz_id:-?}) ..."

{
  echo "# Generated by scripts/install-node.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  [[ -n "$network_id" ]] && echo "# network_id=${network_id}"
  [[ -n "$pkg_version" ]] && echo "# package_version=${pkg_version}"
  echo "POSTGRES_PASSWORD=${postgres_password}"
  echo "DATABASE_URL=postgres://quazar:${postgres_password}@postgres:5432/quazar"
  echo "QUAZAR_NODE_SECRET=${node_secret}"
  echo "QUAZAR_REG_SECRET=${reg_secret}"
  echo "QUAZAR_STRICT_SECRETS=true"
  echo "QUAZAR_NODE_ID=${node_id}"
  echo "QUAZAR_NODE_URL=${node_url}"
  echo "QUAZAR_PORT=8080"
  echo "QUAZAR_INIT_TEST_KEYS=false"
  echo "QUAZAR_SYNC_INTERVAL_SECS=60"
  echo "QUAZAR_CORS_ORIGINS=https://quazar.website"
  echo "ADMIN_API_KEY=${admin_key}"
  if [[ -n "$bootstrap_peers" ]]; then
    echo "QUAZAR_BOOTSTRAP_PEERS=${bootstrap_peers}"
  fi
  echo "RECAPTCHA_SITE_KEY="
  echo "RECAPTCHA_SECRET_KEY="
  echo "QUAZAR_TRUSTED_PROXIES="
  echo "RUST_LOG=info"
} >"$ENV_FILE"

chmod 600 "$ENV_FILE" 2>/dev/null || true

cd "$INSTALL_DIR"

if docker compose version >/dev/null 2>&1; then
  COMPOSE=(docker compose)
elif command -v docker-compose >/dev/null 2>&1; then
  COMPOSE=(docker-compose)
else
  die "neither 'docker compose' nor 'docker-compose' found"
fi

echo "Starting stack: ${COMPOSE[*]} up -d ..."
"${COMPOSE[@]}" up -d

echo "Done. Node ${node_id} should listen on ${node_url} (check: curl -sf ${node_url}/status)."
