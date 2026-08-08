#!/usr/bin/env bash
# Run Quazar stack with plain `docker run` (no Compose plugin required).
# Usage:
#   cp .env.example .env   # edit secrets
#   ./scripts/run-stack-docker.sh up
#   ./scripts/run-stack-docker.sh smoke
#   ./scripts/run-stack-docker.sh down
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

usage() {
  cat <<EOF
Usage: $0 <up|down|status|smoke|hints>

  up      create network/volumes, start postgres + quazar-node
  down    remove containers (keep volumes/network)
  status  show container state
  smoke   curl /status and /nodes
  hints   print smoke commands only

Env: copy .env.example → .env. Set SKIP_BUILD=1 to reuse an existing image.
EOF
}

if [[ $# -lt 1 ]]; then
  usage
  exit 1
fi

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
elif [[ "$1" != "hints" ]]; then
  echo "WARN: .env not found — using defaults from .env.example values." >&2
fi

QUAZAR_DB_PATH="${QUAZAR_DB_PATH:-/data/quazar.db}"
QUAZAR_NODE_ID="${QUAZAR_NODE_ID:-QZ-NODE}"
QUAZAR_NODE_URL="${QUAZAR_NODE_URL:-http://127.0.0.1:8080}"
QUAZAR_PORT="${QUAZAR_PORT:-8080}"
QUAZAR_HOST_BIND="${QUAZAR_HOST_BIND:-127.0.0.1}"
QUAZAR_HOST_PORT="${QUAZAR_HOST_PORT:-8080}"
QUAZAR_NETWORK="${QUAZAR_NETWORK:-quazar-net}"
QUAZAR_NODE_CONTAINER="${QUAZAR_NODE_CONTAINER:-quazar-node}"
QUAZAR_NODE_IMAGE="${QUAZAR_NODE_IMAGE:-quazar-node:local}"
QUAZAR_NODE_VOLUME="${QUAZAR_NODE_VOLUME:-quazar-nodedata}"
POSTGRES_CONTAINER="${POSTGRES_CONTAINER:-quazar-postgres}"
POSTGRES_IMAGE="${POSTGRES_IMAGE:-postgres:16-alpine}"
POSTGRES_VOLUME="${POSTGRES_VOLUME:-quazar-pgdata}"
POSTGRES_HOST_PORT="${POSTGRES_HOST_PORT:-5432}"
POSTGRES_DB="${POSTGRES_DB:-quazar}"
POSTGRES_USER="${POSTGRES_USER:-quazar}"
POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-change-me}"

need_docker() {
  if ! command -v docker >/dev/null 2>&1; then
    echo "ERROR: docker not found in PATH" >&2
    exit 1
  fi
  if ! docker info >/dev/null 2>&1; then
    echo "ERROR: cannot talk to Docker daemon (permission or service down)" >&2
    exit 1
  fi
}

ensure_network() {
  if ! docker network inspect "$QUAZAR_NETWORK" >/dev/null 2>&1; then
    echo "Creating network $QUAZAR_NETWORK"
    docker network create "$QUAZAR_NETWORK" >/dev/null
  fi
}

ensure_volumes() {
  docker volume create "$POSTGRES_VOLUME" >/dev/null
  docker volume create "$QUAZAR_NODE_VOLUME" >/dev/null
}

container_running() {
  local name="$1"
  [[ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null || echo false)" == "true" ]]
}

start_postgres() {
  if container_running "$POSTGRES_CONTAINER"; then
    echo "Postgres already running: $POSTGRES_CONTAINER"
    return 0
  fi
  if docker inspect "$POSTGRES_CONTAINER" >/dev/null 2>&1; then
    echo "Starting existing container $POSTGRES_CONTAINER"
    docker start "$POSTGRES_CONTAINER" >/dev/null
    return 0
  fi
  echo "Starting $POSTGRES_CONTAINER ($POSTGRES_IMAGE)"
  docker run -d \
    --name "$POSTGRES_CONTAINER" \
    --network "$QUAZAR_NETWORK" \
    --restart unless-stopped \
    -e POSTGRES_DB="$POSTGRES_DB" \
    -e POSTGRES_USER="$POSTGRES_USER" \
    -e POSTGRES_PASSWORD="$POSTGRES_PASSWORD" \
    -p "${QUAZAR_HOST_BIND}:${POSTGRES_HOST_PORT}:5432" \
    -v "${POSTGRES_VOLUME}:/var/lib/postgresql/data" \
    "$POSTGRES_IMAGE" >/dev/null
}

build_node_image() {
  echo "Building image $QUAZAR_NODE_IMAGE (may take several minutes)"
  docker build -t "$QUAZAR_NODE_IMAGE" .
}

start_node() {
  if container_running "$QUAZAR_NODE_CONTAINER"; then
    echo "Node already running: $QUAZAR_NODE_CONTAINER"
    return 0
  fi
  if docker inspect "$QUAZAR_NODE_CONTAINER" >/dev/null 2>&1; then
    echo "Removing stopped container $QUAZAR_NODE_CONTAINER to apply env/image"
    docker rm "$QUAZAR_NODE_CONTAINER" >/dev/null
  fi
  echo "Starting $QUAZAR_NODE_CONTAINER ($QUAZAR_NODE_IMAGE)"
  # Current binary stores state in SQLite at QUAZAR_DB_PATH (not Postgres).
  docker run -d \
    --name "$QUAZAR_NODE_CONTAINER" \
    --network "$QUAZAR_NETWORK" \
    --restart unless-stopped \
    -e QUAZAR_DB_PATH="$QUAZAR_DB_PATH" \
    -e QUAZAR_NODE_ID="$QUAZAR_NODE_ID" \
    -e QUAZAR_NODE_URL="$QUAZAR_NODE_URL" \
    -e QUAZAR_PORT="$QUAZAR_PORT" \
    -p "${QUAZAR_HOST_BIND}:${QUAZAR_HOST_PORT}:${QUAZAR_PORT}" \
    -v "${QUAZAR_NODE_VOLUME}:/data" \
    "$QUAZAR_NODE_IMAGE" >/dev/null
}

cmd_up() {
  need_docker
  ensure_network
  ensure_volumes
  start_postgres
  if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    build_node_image
  fi
  start_node
  echo
  echo "Stack is up."
  echo "  network:   $QUAZAR_NETWORK"
  echo "  postgres:  $POSTGRES_CONTAINER  (${QUAZAR_HOST_BIND}:${POSTGRES_HOST_PORT})"
  echo "  node:      $QUAZAR_NODE_CONTAINER  (${QUAZAR_HOST_BIND}:${QUAZAR_HOST_PORT})"
  echo
  print_smoke_hints
}

cmd_down() {
  need_docker
  for name in "$QUAZAR_NODE_CONTAINER" "$POSTGRES_CONTAINER"; do
    if docker inspect "$name" >/dev/null 2>&1; then
      echo "Stopping/removing $name"
      docker rm -f "$name" >/dev/null || true
    fi
  done
  echo "Containers removed. Volumes ($POSTGRES_VOLUME, $QUAZAR_NODE_VOLUME) and network ($QUAZAR_NETWORK) kept."
}

cmd_status() {
  need_docker
  echo "Containers:"
  docker ps -a --filter "name=^/${QUAZAR_NODE_CONTAINER}$" --filter "name=^/${POSTGRES_CONTAINER}$" \
    --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
}

print_smoke_hints() {
  local base="http://${QUAZAR_HOST_BIND}:${QUAZAR_HOST_PORT}"
  cat <<EOF
Smoke (after node is healthy):
  curl -sS ${base}/status
  curl -sS ${base}/nodes
  curl -sS ${base}/blocks

Notes:
  - Do not hit Hermes https://quazar.website/health from this script.
  - Current quazar_registry uses SQLite; Postgres is a sidecar for [QUA-10] alignment.
  - Exchange: /exchange/requests, /exchange/listings, /exchange/security/tasks, /svod
EOF
}

cmd_smoke() {
  local base="http://${QUAZAR_HOST_BIND}:${QUAZAR_HOST_PORT}"
  echo "GET ${base}/status"
  curl -sS --max-time 5 "${base}/status" || true
  echo
  echo "GET ${base}/nodes"
  curl -sS --max-time 5 "${base}/nodes" || true
  echo
}

case "${1:-}" in
  up) cmd_up ;;
  down) cmd_down ;;
  status) cmd_status ;;
  smoke) cmd_smoke ;;
  hints) print_smoke_hints ;;
  *) usage; exit 1 ;;
esac
