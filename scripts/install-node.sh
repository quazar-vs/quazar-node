#!/usr/bin/env bash
# =============================================================================
# install-node.sh — автоматическое развёртывание узла Квазара (мэр / peer)
#
# Назначение:
#   Загружает установочный пакет из сети, создаёт .env, поднимает Docker Compose
#   и регистрирует узел через POST /api/mayor/node-started.
#
# Аргументы:
#   --ip ADDR        Публичный IP узла (обязательный)
#   --api-key KEY    API-ключ гражданина — владельца заявки (обязательный)
#   --peer URL       Опционально: URL пира (переопределяет peer из пакета)
#   --help           Справка
#
# Дополнительно (не в справке пользователя, для автоматизации):
#   --network-url URL   Базовый URL реестра (или QUAZAR_NETWORK_URL)
#   --dir PATH          Каталог установки с docker-compose.yml (по умолчанию: корень репо)
#   --force             Перезаписать .env без вопроса
#   --skip-register     Не вызывать /api/mayor/node-started
#
# Секреты (node_secret, reg_secret, пароли) в stdout не выводятся.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_NETWORK_URL="${QUAZAR_NETWORK_URL:-https://quazar.website:8443}"
REPO_URL="${QUAZAR_REPO_URL:-https://github.com/quazar-vs/quazar-node.git}"
REPO_BRANCH="${QUAZAR_REPO_BRANCH:-main}"

NODE_IP=""
API_KEY=""
PEER_URL=""
NETWORK_URL="$DEFAULT_NETWORK_URL"
INSTALL_DIR="$ROOT"
FORCE=0
SKIP_REGISTER=0
WAIT_SECS=180

usage() {
  cat <<'EOF'
Usage: install-node.sh --ip PUBLIC_IP --api-key KEY [options]

Обязательные:
  --ip ADDR              Публичный IP сервера (как в заявке мэра)
  --api-key KEY          API-ключ гражданина (владельца заявки)

Опционально:
  --peer URL             URL пира для синхронизации (если не брать из пакета)
  --help                 Эта справка

Переменные окружения:
  QUAZAR_NETWORK_URL     URL сети (по умолчанию https://quazar.website:8443)
  QUAZAR_REPO_URL        Git remote (по умолчанию github.com/quazar-vs/quazar-node)
  RECAPTCHA_SITE_KEY     Нужны оба ключа при QUAZAR_STRICT_SECRETS=true
  RECAPTCHA_SECRET_KEY

Пример:
  ./scripts/install-node.sh --ip 203.0.113.10 --api-key qz_...
EOF
}

log() {
  echo "install-node: $*"
}

die() {
  echo "install-node: ОШИБКА: $*" >&2
  exit 1
}

require_root_for_docker_install() {
  if [[ "$(id -u)" -ne 0 ]]; then
    die "для установки Docker нужны права root (sudo ./scripts/install-node.sh ...)"
  fi
}

# --- Парсинг аргументов ---
while [[ $# -gt 0 ]]; do
  case "$1" in
    --ip)
      [[ $# -ge 2 ]] || die "--ip требует значение"
      NODE_IP="$2"
      shift 2
      ;;
    --api-key)
      [[ $# -ge 2 ]] || die "--api-key требует значение"
      API_KEY="$2"
      shift 2
      ;;
    --peer)
      [[ $# -ge 2 ]] || die "--peer требует значение"
      PEER_URL="$2"
      shift 2
      ;;
    --network-url)
      [[ $# -ge 2 ]] || die "--network-url требует значение"
      NETWORK_URL="$2"
      shift 2
      ;;
    --dir)
      [[ $# -ge 2 ]] || die "--dir требует значение"
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
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "неизвестный аргумент: $1 (см. --help)"
      ;;
  esac
done

# --- Валидация обязательных полей ---
[[ -n "${NODE_IP// }" ]] || die "укажите --ip"
[[ -n "${API_KEY// }" ]] || die "укажите --api-key"

# --- Установка системных зависимостей (curl, jq) ---
install_apt_packages() {
  local pkgs=("$@")
  if command -v apt-get >/dev/null 2>&1; then
    require_root_for_docker_install
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq "${pkgs[@]}"
    return 0
  fi
  return 1
}

ensure_curl_jq() {
  if ! command -v curl >/dev/null 2>&1; then
    install_apt_packages curl || die "установите curl вручную"
  fi
  if ! command -v jq >/dev/null 2>&1; then
    install_apt_packages jq || die "установите jq вручную"
  fi
}

# --- Установка Docker и Compose plugin ---
ensure_docker() {
  if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
    return 0
  fi
  if command -v docker >/dev/null 2>&1 && command -v docker-compose >/dev/null 2>&1; then
    return 0
  fi

  log "Docker не найден — попытка установки (Debian/Ubuntu)..."
  require_root_for_docker_install
  install_apt_packages ca-certificates curl gnupg || true
  if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com | sh
  fi
  systemctl enable --now docker 2>/dev/null || true
  command -v docker >/dev/null 2>&1 || die "не удалось установить Docker"
  docker compose version >/dev/null 2>&1 || command -v docker-compose >/dev/null 2>&1 \
    || die "установите Docker Compose plugin (docker compose)"
}

compose_cmd() {
  if docker compose version >/dev/null 2>&1; then
    echo "docker compose"
  elif command -v docker-compose >/dev/null 2>&1; then
    echo "docker-compose"
  else
    die "не найден docker compose"
  fi
}

# --- Клонирование / обновление репозитория ---
ensure_repository() {
  INSTALL_DIR="$(cd "$INSTALL_DIR" 2>/dev/null && pwd || true)"
  if [[ -z "$INSTALL_DIR" ]]; then
    INSTALL_DIR="$ROOT"
  fi

  if [[ -f "$INSTALL_DIR/docker-compose.yml" ]]; then
    if [[ -d "$INSTALL_DIR/.git" ]]; then
      log "Обновление репозитория в ${INSTALL_DIR} ..."
      git -C "$INSTALL_DIR" fetch origin "$REPO_BRANCH" 2>/dev/null || true
      git -C "$INSTALL_DIR" pull --ff-only origin "$REPO_BRANCH" 2>/dev/null \
        || log "git pull пропущен (локальные изменения или нет сети)"
    fi
    return 0
  fi

  log "Клонирование ${REPO_URL} → ${INSTALL_DIR} ..."
  mkdir -p "$(dirname "$INSTALL_DIR")"
  git clone --branch "$REPO_BRANCH" --depth 1 "$REPO_URL" "$INSTALL_DIR"
}

strip_0x() {
  local s="$1"
  case "$s" in
    0x* | 0X*) echo "${s#??}" ;;
    *) echo "$s" ;;
  esac
}

urlencode() {
  local raw="$1"
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=""))' "$raw"
  elif command -v jq >/dev/null 2>&1; then
    jq -rn --arg v "$raw" '$v|@uri'
  else
    die "нужен python3 или jq для кодирования пароля DATABASE_URL"
  fi
}

postgres_db_user() {
  local qz="$1"
  local user
  if [[ "$qz" == qz_* ]]; then
    user="$qz"
  else
    user="qz_${qz}"
  fi
  user="$(echo "$user" | tr -cd 'a-zA-Z0-9_' | head -c 63)"
  [[ -n "$user" ]] || user="qz_node"
  echo "$user"
}

generate_hex() {
  local nbytes="${1:-32}"
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex "$nbytes"
  else
    head -c "$nbytes" /dev/urandom | xxd -p -c 256 | head -c $((nbytes * 2))
  fi
}

generate_admin_api_key() {
  echo "qz_$(generate_hex 16)"
}

# --- Начало установки ---
ensure_curl_jq
ensure_docker
ensure_repository

INSTALL_DIR="$(cd "$INSTALL_DIR" && pwd)"
[[ -f "$INSTALL_DIR/docker-compose.yml" ]] || die "docker-compose.yml не найден в ${INSTALL_DIR}"

ENV_FILE="$INSTALL_DIR/.env"
OVERRIDE_SRC="$INSTALL_DIR/scripts/docker-compose.mayor-node.override.yml"
OVERRIDE_DST="$INSTALL_DIR/docker-compose.override.yml"

# --- Повторный запуск: .env и контейнеры ---
COMPOSE="$(compose_cmd)"
cd "$INSTALL_DIR"

if [[ -f "$ENV_FILE" && "$FORCE" -ne 1 ]]; then
  if [[ -t 0 ]]; then
    echo "Файл ${ENV_FILE} уже существует."
    read -r -p "Перезаписать? [y/N]: " ans
    case "$ans" in
      y | Y | yes | YES) ;;
      *) die "выход без изменений (используйте --force для перезаписи без вопроса)" ;;
    esac
  else
    die ".env уже существует (используйте --force)"
  fi
fi

running="$(eval "$COMPOSE ps --status running -q" 2>/dev/null | wc -l)"
if [[ "${running// /}" -gt 0 ]]; then
  log "Уже запущенные контейнеры:"
  eval "$COMPOSE ps"
fi

# --- Override: публичный :8080 ---
if [[ -f "$OVERRIDE_SRC" ]]; then
  cp "$OVERRIDE_SRC" "$OVERRIDE_DST"
  log "Применён ${OVERRIDE_DST} (порт 8080)"
fi

NETWORK_URL="${NETWORK_URL%/}"
SETUP_URL="${NETWORK_URL}/api/mayor/setup-package"

log "Запрос установочного пакета: ${SETUP_URL} (ip=${NODE_IP})"

HTTP_BODY=""
HTTP_CODE=""
if ! HTTP_BODY="$(curl -sS -G \
  --connect-timeout 30 \
  --max-time 120 \
  "$SETUP_URL" \
  --data-urlencode "ip=${NODE_IP}" \
  --data-urlencode "api_key=${API_KEY}" \
  -H "Accept: application/json" \
  -w $'\n__HTTP_CODE__%{http_code}')" 2>/dev/null; then
  die "не удалось подключиться к сети (${NETWORK_URL}). Проверьте URL и доступность."
fi

HTTP_CODE="${HTTP_BODY##*$'\n__HTTP_CODE__'}"
HTTP_BODY="${HTTP_BODY%$'\n__HTTP_CODE__'*}"

if [[ "$HTTP_CODE" != "200" ]]; then
  err_msg="$(echo "$HTTP_BODY" | jq -r '.error // empty' 2>/dev/null || true)"
  if [[ -n "$err_msg" ]]; then
    die "setup-package HTTP ${HTTP_CODE}: ${err_msg}"
  fi
  die "setup-package HTTP ${HTTP_CODE}"
fi

API_STATUS="$(echo "$HTTP_BODY" | jq -r '.status // empty')"
if [[ "$API_STATUS" != "success" ]]; then
  err_msg="$(echo "$HTTP_BODY" | jq -r '.error // "неизвестная ошибка"')"
  die "сеть отклонила запрос: ${err_msg}"
fi

PKG="$HTTP_BODY"
node_id="$(echo "$PKG" | jq -r '.data.node_id // empty')"
node_secret="$(echo "$PKG" | jq -r '.data.node_secret // empty')"
sync_peer="$(echo "$PKG" | jq -r '.data.sync_peer // empty')"
peer_url_pkg="$(echo "$PKG" | jq -r '.data.peer_url // empty')"
network_id="$(echo "$PKG" | jq -r '.data.network_id // empty')"
pkg_version="$(echo "$PKG" | jq -r '.data.version // empty')"
citizen_name="$(echo "$PKG" | jq -r '.data.citizen_name // empty')"
qz_id="$(echo "$PKG" | jq -r '.data.qz_id // empty')"

[[ -n "$node_id" ]] || die "в пакете нет node_id"
[[ -n "$node_secret" ]] || die "в пакете нет node_secret"

node_secret_for_register="$node_secret"
node_secret="$(strip_0x "$node_secret")"

# Локальный reg_secret (в API пакета не передаётся)
reg_secret="$(generate_hex 32)"
admin_key="$(generate_admin_api_key)"

peer_url="${PEER_URL:-$peer_url_pkg}"
if [[ -n "$PEER_URL" ]]; then
  log "Используется --peer: ${peer_url}"
fi

bootstrap_peers=""
if [[ -n "$PEER_URL" ]]; then
  if [[ "$PEER_URL" == *"@"* ]]; then
    bootstrap_peers="$PEER_URL"
  elif [[ -n "$sync_peer" && "$sync_peer" == *"@"* ]]; then
    bootstrap_peers="${sync_peer%%@*}@${PEER_URL}"
  else
    log "предупреждение: --peer без id@url и без sync_peer — задайте QUAZAR_BOOTSTRAP_PEERS вручную"
  fi
elif [[ -n "$sync_peer" ]]; then
  bootstrap_peers="$sync_peer"
fi

db_user="$(postgres_db_user "$qz_id")"
db_pass="$API_KEY"
db_pass_enc="$(urlencode "$db_pass")"
database_url="postgres://${db_user}:${db_pass_enc}@postgres:5432/quazar"

node_url="http://${NODE_IP}:8080"

recaptcha_site="${RECAPTCHA_SITE_KEY:-}"
recaptcha_secret="${RECAPTCHA_SECRET_KEY:-}"
strict_secrets="true"
if [[ -z "$recaptcha_site" || -z "$recaptcha_secret" ]]; then
  die "QUAZAR_STRICT_SECRETS=true требует RECAPTCHA_SITE_KEY и RECAPTCHA_SECRET_KEY в окружении перед запуском скрипта"
fi

log "Запись ${ENV_FILE} (node_id=${node_id}, гражданин=${citizen_name:-?}, network=${network_id:-?}) ..."

umask 077
{
  echo "# Сгенерировано scripts/install-node.sh $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "POSTGRES_USER=${db_user}"
  echo "POSTGRES_PASSWORD=${db_pass}"
  echo "DATABASE_URL=${database_url}"
  echo "QUAZAR_NODE_ID=${node_id}"
  echo "QUAZAR_NODE_SECRET=${node_secret}"
  echo "QUAZAR_REG_SECRET=${reg_secret}"
  echo "QUAZAR_NETWORK_ID=${network_id}"
  echo "QUAZAR_VERSION=${pkg_version}"
  echo "QUAZAR_PEER_URL=${peer_url}"
  echo "QUAZAR_NODE_URL=${node_url}"
  echo "QUAZAR_PORT=8080"
  echo "QUAZAR_NETWORK_URL=${NETWORK_URL}"
  echo "QUAZAR_STRICT_SECRETS=${strict_secrets}"
  echo "QUAZAR_INIT_TEST_KEYS=false"
  echo "ADMIN_API_KEY=${admin_key}"
  echo "RECAPTCHA_SITE_KEY=${recaptcha_site}"
  echo "RECAPTCHA_SECRET_KEY=${recaptcha_secret}"
  if [[ -n "$bootstrap_peers" ]]; then
    echo "QUAZAR_BOOTSTRAP_PEERS=${bootstrap_peers}"
  fi
  echo "QUAZAR_SYNC_INTERVAL_SECS=60"
  echo "QUAZAR_CORS_ORIGINS=https://quazar.website"
  echo "QUAZAR_TRUSTED_PROXIES="
  echo "RUST_LOG=info"
} >"$ENV_FILE"

chmod 600 "$ENV_FILE" 2>/dev/null || true

# --- Docker Compose: postgres + API ---
log "Запуск: ${COMPOSE} up -d --build postgres quazar-node"
eval "$COMPOSE up -d --build postgres quazar-node"

wait_for_local_status() {
  local deadline=$((SECONDS + WAIT_SECS))
  local url="http://127.0.0.1:8080/status"
  log "Ожидание готовности ${url} (до ${WAIT_SECS} с) ..."
  while (( SECONDS < deadline )); do
    if curl -sf "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 3
  done
  return 1
}

register_node_started() {
  local register_url="${NETWORK_URL}/api/mayor/node-started"
  log "Регистрация узла: POST ${register_url} (node_id=${node_id})"
  local body
  body="$(jq -n \
    --arg node_id "$node_id" \
    --arg node_secret "$node_secret_for_register" \
    --arg url "$node_url" \
    '{node_id: $node_id, node_secret: $node_secret, url: $url}')"

  local resp code
  if ! resp="$(curl -sS --connect-timeout 30 --max-time 120 -X POST "$register_url" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d "$body" \
    -w $'\n__HTTP_CODE__%{http_code}')" 2>/dev/null; then
    die "не удалось вызвать node-started (проверьте сеть)"
  fi

  code="${resp##*$'\n__HTTP_CODE__'}"
  resp="${resp%$'\n__HTTP_CODE__'*}"

  if [[ "$code" != "200" ]]; then
    err_msg="$(echo "$resp" | jq -r '.error // empty' 2>/dev/null || true)"
    if [[ -n "$err_msg" ]]; then
      die "node-started HTTP ${code}: ${err_msg}"
    fi
    die "node-started HTTP ${code}"
  fi

  local st
  st="$(echo "$resp" | jq -r '.data.status // .status // empty')"
  log "Узел зарегистрирован в сети (status=${st:-ok})."
}

if [[ "$SKIP_REGISTER" -eq 0 ]]; then
  if wait_for_local_status; then
    register_node_started
  else
    die "узел не ответил вовремя; контейнеры запущены — повторите регистрацию вручную (POST ${NETWORK_URL}/api/mayor/node-started)"
  fi
else
  log "Регистрация пропущена (--skip-register)."
fi

cat <<EOF

================================================================================
  Установка узла Квазара завершена
================================================================================
  Node ID:     ${node_id}
  Публичный URL: ${node_url}
  Сеть:        ${NETWORK_URL}
  Проверка:    curl -sf ${node_url}/status

  Для мэра:
    1. Убедитесь, что порт 8080 открыт в файрволе и доступен из интернета.
    2. В панели сети дождитесь подтверждения узла (node-started отправлен).
    3. ADMIN_API_KEY сгенерирован локально — храните в ${ENV_FILE} (не публикуйте).
    4. Для исходящей почты (коды, credentials) настройте SMTP_* / NOTIFY_EMAIL в .env.

================================================================================
EOF
