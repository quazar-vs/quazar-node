#!/usr/bin/env bash
# Install a Quazar mayor node from a network setup package (non-interactive).
#
# Flow:
#   1. GET {network}/api/mayor/setup-package?ip=<IP>&api_key=<KEY>
#   2. Write .env from package (node_id, secrets, sync_peer, …) — secrets are not echoed
#   3. docker compose up -d (or docker-compose)
#   4. POST {network}/api/mayor/node-started  { node_id, node_secret, url }
#
# Network base URL (pick one):
#   --network-url URL
#   QUAZAR_NETWORK_URL   (default: https://quazar.website)
#
# API key (Founder/Aiya/citizen key that owns the pending mayor application for --ip):
#   --api-key KEY | QUAZAR_API_KEY
#
# Example:
#   export QUAZAR_API_KEY='qz_api_...'
#   ./scripts/install-node.sh --ip 203.0.113.10
#   ./scripts/install-node.sh --ip 203.0.113.10 --api-key "$QUAZAR_API_KEY" \
#       --network-url https://quazar.website --force
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_NETWORK_URL="${QUAZAR_NETWORK_URL:-https://quazar.website}"

NODE_IP=""
API_KEY="${QUAZAR_API_KEY:-}"
NETWORK_URL="$DEFAULT_NETWORK_URL"
INSTALL_DIR="$ROOT"
FORCE=0
SKIP_REGISTER=0
WAIT_SECS=120

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
  --skip-register        Do not POST /api/mayor/node-started after start
  --wait SECS            Seconds to wait for local /status before register (default 120)
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
    --skip-register)
      SKIP_REGISTER=1
      shift
      ;;
    --wait)
      [[ $# -ge 2 ]] || die "--wait requires a value"
      WAIT_SECS="$2"
      shift 2
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
[[ -n "$API_KEY" ]] || die "API key missing: pass --api-key or set QUAZAR_API_KEY"

require_cmd curl
require_cmd jq
require_cmd docker

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
# Exact query form expected by MayorSetupPackageQuery: ip + api_key
if ! HTTP_BODY="$(curl -sS -G \
  "$SETUP_URL" \
  --data-urlencode "ip=${NODE_IP}" \
  --data-urlencode "api_key=${API_KEY}" \
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

# Fields from MayorSetupPackageResponse (under ApiResponse.data)
PKG="$HTTP_BODY"
node_id="$(echo "$PKG" | jq -r '.data.node_id // empty')"
node_secret="$(echo "$PKG" | jq -r '.data.node_secret // empty')"
sync_peer="$(echo "$PKG" | jq -r '.data.sync_peer // empty')"
peer_url="$(echo "$PKG" | jq -r '.data.peer_url // empty')"
network_id="$(echo "$PKG" | jq -r '.data.network_id // empty')"
pkg_version="$(echo "$PKG" | jq -r '.data.version // empty')"
citizen_name="$(echo "$PKG" | jq -r '.data.citizen_name // empty')"
qz_id="$(echo "$PKG" | jq -r '.data.qz_id // empty')"

[[ -n "$node_id" ]] || die "package missing node_id"
[[ -n "$node_secret" ]] || die "package missing node_secret"

# Keep package form for node-started; strip 0x for local env (node_secrets_match normalizes).
node_secret_for_register="$node_secret"
node_secret="$(strip_0x "$node_secret")"

# Never use the network's QUAZAR_REG_SECRET — generate a local one for this mayor node.
if command -v openssl >/dev/null 2>&1; then
  reg_secret="$(openssl rand -hex 32)"
else
  reg_secret="$(head -c 32 /dev/urandom | xxd -p -c 256 | head -c 64)"
fi
[[ -n "$reg_secret" ]] || die "failed to generate local reg_secret"

# API key was already supplied to fetch the package; do not echo it from the response.
admin_key="$API_KEY"

# QUAZAR_BOOTSTRAP_PEERS expects id@url (see README / main.rs bootstrap parser).
bootstrap_peers=""
if [[ -n "$sync_peer" ]]; then
  bootstrap_peers="$sync_peer"
elif [[ "$peer_url" == *"@"* ]]; then
  bootstrap_peers="$peer_url"
elif [[ -n "$peer_url" ]]; then
  echo "install-node: warning: no sync_peer in package; peer_url is bare URL — bootstrap may need manual QUAZAR_BOOTSTRAP_PEERS" >&2
fi

if command -v openssl >/dev/null 2>&1; then
  postgres_password="$(openssl rand -base64 32 | tr -d '\n' | tr '/+' '_-' | head -c 32)"
else
  postgres_password="$(head -c 32 /dev/urandom | base64 | tr -d '\n' | tr '/+' '_-' | head -c 32)"
fi
node_url="http://${NODE_IP}:8080"

# STRICT mode requires both reCAPTCHA keys at startup; leave unset unless provided.
recaptcha_site="${RECAPTCHA_SITE_KEY:-}"
recaptcha_secret="${RECAPTCHA_SECRET_KEY:-}"
if [[ -n "$recaptcha_site" && -n "$recaptcha_secret" ]]; then
  strict_secrets="true"
else
  strict_secrets="false"
  if [[ -z "$recaptcha_site" && -z "$recaptcha_secret" ]]; then
    echo "install-node: RECAPTCHA keys unset — QUAZAR_STRICT_SECRETS=false (set both keys to enable STRICT)" >&2
  else
    echo "install-node: warning: only one RECAPTCHA key set — STRICT disabled until both are present" >&2
  fi
fi

echo "Writing ${ENV_FILE} (node_id=${node_id}, citizen=${citizen_name:-?}, qz_id=${qz_id:-?}) ..."

umask 077
{
  echo "# Generated by scripts/install-node.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  [[ -n "$network_id" ]] && echo "# network_id=${network_id}"
  [[ -n "$pkg_version" ]] && echo "# package_version=${pkg_version}"
  echo "POSTGRES_PASSWORD=${postgres_password}"
  echo "DATABASE_URL=postgres://quazar:${postgres_password}@postgres:5432/quazar"
  echo "QUAZAR_NODE_SECRET=${node_secret}"
  echo "QUAZAR_REG_SECRET=${reg_secret}"
  echo "QUAZAR_STRICT_SECRETS=${strict_secrets}"
  echo "QUAZAR_NODE_ID=${node_id}"
  echo "QUAZAR_NODE_URL=${node_url}"
  echo "QUAZAR_PORT=8080"
  echo "QUAZAR_INIT_TEST_KEYS=false"
  echo "QUAZAR_SYNC_INTERVAL_SECS=60"
  echo "QUAZAR_CORS_ORIGINS=https://quazar.website"
  echo "QUAZAR_NETWORK_URL=${NETWORK_URL}"
  echo "ADMIN_API_KEY=${admin_key}"
  if [[ -n "$bootstrap_peers" ]]; then
    echo "QUAZAR_BOOTSTRAP_PEERS=${bootstrap_peers}"
  fi
  echo "RECAPTCHA_SITE_KEY=${recaptcha_site}"
  echo "RECAPTCHA_SECRET_KEY=${recaptcha_secret}"
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

wait_for_local_status() {
  local deadline=$((SECONDS + WAIT_SECS))
  local url="http://127.0.0.1:8080/status"
  echo "Waiting for local node ${url} (up to ${WAIT_SECS}s) ..."
  while (( SECONDS < deadline )); do
    if curl -sf "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  return 1
}

register_node_started() {
  local register_url="${NETWORK_URL}/api/mayor/node-started"
  echo "Registering node with network: POST ${register_url} (node_id=${node_id}) ..."
  local body
  body="$(jq -n \
    --arg node_id "$node_id" \
    --arg node_secret "$node_secret_for_register" \
    --arg url "$node_url" \
    '{node_id: $node_id, node_secret: $node_secret, url: $url}')"

  local resp code
  if ! resp="$(curl -sS -X POST "$register_url" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d "$body" \
    -w $'\n__HTTP_CODE__%{http_code}')" ; then
    die "node-started request failed (check network URL and connectivity)"
  fi

  code="${resp##*$'\n__HTTP_CODE__'}"
  resp="${resp%$'\n__HTTP_CODE__'*}"

  if [[ "$code" != "200" ]]; then
    err_msg="$(echo "$resp" | jq -r '.error // empty' 2>/dev/null || true)"
    if [[ -n "$err_msg" ]]; then
      die "node-started HTTP ${code}: ${err_msg}"
    fi
    die "node-started HTTP ${code}: ${resp}"
  fi

  local st
  st="$(echo "$resp" | jq -r '.data.status // .status // empty')"
  echo "Node registered (status=${st:-ok})."
}

if [[ "$SKIP_REGISTER" -eq 0 ]]; then
  if wait_for_local_status; then
    register_node_started
  else
    die "local node did not become ready in time; stack is up — retry register with: curl -sS -X POST ${NETWORK_URL}/api/mayor/node-started -H 'Content-Type: application/json' -d '{\"node_id\":\"${node_id}\",\"url\":\"${node_url}\",\"node_secret\":\"…\"}'"
  fi
else
  echo "Skipping network registration (--skip-register)."
fi

echo "Done. Node ${node_id} should listen on ${node_url} (check: curl -sf ${node_url}/status)."
