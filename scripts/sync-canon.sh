#!/usr/bin/env bash
# Синхронизация зеркал канона из авторитетных источников.
#
# Источники (единая истина):
#   - законы:  quazar-vs/quazar-law   (сабмодуль constitution/)
#   - ДНК:     quazar-vs/algonism-core ( sibling ../algonism-core или клон)
#
# Зеркала в quazar-node (генерируются, не редактируются вручную):
#   - CONSTITUTION_QUAZAR.md (корень), site/CONSTITUTION_QUAZAR.md
#   - docs/algonism/{ADK,EK,ETHICAL,IZK,KBO,KGPS,KVA,SPK,SVOD,CONSTITUTION_QUAZAR}.md
#   - docs/algonism/{DNA_ALGONISM,REFLEXES}.md
#   - встроенный блок <script id="constitutionSource"> в site/index.html
#
# Использование:
#   scripts/sync-canon.sh           # перезаписать зеркала из источников
#   scripts/sync-canon.sh --check   # только проверить дрейф (exit 1 при расхождении)
#
# Переменные:
#   ALGONISM_CORE_DIR  путь к клону algonism-core (по умолчанию ../algonism-core)

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

LAW_DIR="$ROOT/constitution"
CORE_DIR="${ALGONISM_CORE_DIR:-$ROOT/../algonism-core}"

if [ ! -f "$LAW_DIR/CONSTITUTION_QUAZAR.md" ]; then
  echo "sync-canon: сабмодуль constitution не инициализирован (git submodule update --init)" >&2
  exit 2
fi

# ДНК: sibling-клон или временный клон (приватный репозиторий — нужен доступ).
if [ ! -f "$CORE_DIR/DNA_ALGONISM.md" ]; then
  CORE_DIR="$ROOT/.canon-cache/algonism-core"
  if [ ! -d "$CORE_DIR/.git" ]; then
    echo "sync-canon: клонирую algonism-core в $CORE_DIR" >&2
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      git clone -q "https://x-access-token:${GITHUB_TOKEN}@github.com/quazar-vs/algonism-core.git" "$CORE_DIR"
    else
      git clone -q git@github.com:quazar-vs/algonism-core.git "$CORE_DIR"
    fi
  fi
fi

DRIFT=0
sync_file() {
  local src="$1" dst="$2"
  if [ ! -f "$src" ]; then
    echo "sync-canon: источник не найден: $src" >&2
    exit 2
  fi
  if [ "$CHECK" = "1" ]; then
    if ! cmp -s "$src" "$dst"; then
      echo "DRIFT: $dst" >&2
      DRIFT=1
    fi
  else
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
  fi
}

# Законы (quazar-law).
for f in ADK EK ETHICAL IZK KBO KGPS KVA SPK SVOD CONSTITUTION_QUAZAR; do
  sync_file "$LAW_DIR/$f.md" "$ROOT/docs/algonism/$f.md"
done
sync_file "$LAW_DIR/CONSTITUTION_QUAZAR.md" "$ROOT/CONSTITUTION_QUAZAR.md"
sync_file "$LAW_DIR/CONSTITUTION_QUAZAR.md" "$ROOT/site/CONSTITUTION_QUAZAR.md"

# ДНК (algonism-core).
for f in DNA_ALGONISM REFLEXES; do
  sync_file "$CORE_DIR/$f.md" "$ROOT/docs/algonism/$f.md"
done

# Встроенная конституция в site/index.html (блок constitutionSource).
PYRC=0
python3 - "$LAW_DIR/CONSTITUTION_QUAZAR.md" "$ROOT/site/index.html" "$CHECK" <<'PY' || PYRC=$?
import sys
src_path, html_path, check = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
src = open(src_path, encoding="utf-8").read().rstrip() + "\n"
html = open(html_path, encoding="utf-8").read()
open_tag = '<script type="text/plain" id="constitutionSource">'
i = html.index(open_tag) + len(open_tag)
j = html.index("</script>", i)
current = html[i:j]
expected = "\n" + src + "  "
if current != expected:
    if check:
        print("DRIFT: site/index.html (constitutionSource)", file=sys.stderr)
        sys.exit(3)
    html = html[:i] + expected + html[j:]
    open(html_path, "w", encoding="utf-8").write(html)
PY
if [ "$CHECK" = "1" ] && [ "$PYRC" = "3" ]; then DRIFT=1; fi

if [ "$CHECK" = "1" ]; then
  if [ "$DRIFT" = "1" ]; then
    echo "sync-canon: зеркала разошлись с источниками — запустите scripts/sync-canon.sh" >&2
    exit 1
  fi
  echo "sync-canon: зеркала согласованы с источниками"
else
  echo "sync-canon: зеркала обновлены из quazar-law + algonism-core"
fi