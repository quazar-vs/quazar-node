#!/bin/bash
# Replace YouTube OAuth Desktop client secret and (optionally) start reauth on :8765.
# Does not print secrets. Do not commit google_*.json.
set -euo pipefail

HOME_DIR="${QUAZAR_YOUTUBE_HOME:-/var/lib/quazar/youtube}"
SECRET="$HOME_DIR/google_client_secret.json"
TOKEN="$HOME_DIR/google_token.json"
INBOX_DEFAULT="$HOME_DIR/inbox/google_client_secret.json"
PY="${QUAZAR_YOUTUBE_PY:-/var/lib/quazar/venv-youtube/bin/python3}"
PORT="${OAUTH_PORT:-8765}"
STAMP="$(date +%Y%m%d%H%M%S)"

usage() {
  cat <<'U'
Usage:
  quazar-youtube-oauth-swap.sh install <path-to-downloaded.json>
  quazar-youtube-oauth-swap.sh install-inbox   # uses /var/lib/quazar/youtube/inbox/google_client_secret.json
  quazar-youtube-oauth-swap.sh reauth          # start InstalledAppFlow on :8765
  quazar-youtube-oauth-swap.sh all <json>      # install + reauth
  quazar-youtube-oauth-swap.sh all-inbox

Expect Desktop (installed) client JSON from Google Cloud.
Redirect URI in Console: http://127.0.0.1:8765/
U
}

validate_secret() {
  local path="$1"
  "$PY" - "$path" <<'PY'
import json, sys
from pathlib import Path
p = Path(sys.argv[1])
data = json.loads(p.read_text())
if "installed" not in data:
    kinds = ", ".join(sorted(data.keys()))
    print(
        f"ERROR: expected Desktop client JSON with top-level key 'installed' (got: {kinds})",
        file=sys.stderr,
    )
    if "web" in data:
        print(
            "HINT: this looks like a Web client — create OAuth client type Desktop app.",
            file=sys.stderr,
        )
    sys.exit(2)
block = data["installed"]
pid = block.get("project_id", "?")
cid = block.get("client_id", "")
prefix = cid.split("-")[0] if cid else "?"
redirects = block.get("redirect_uris") or []
print(f"ok type=installed project_id={pid} client_id_prefix={prefix} redirect_uris={redirects}")
if not block.get("client_secret"):
    print("ERROR: missing client_secret", file=sys.stderr)
    sys.exit(2)
PY
}

do_install() {
  local src="$1"
  if [[ ! -f "$src" ]]; then
    echo "ERROR: file not found: $src" >&2
    exit 1
  fi
  validate_secret "$src"
  mkdir -p "$HOME_DIR"
  if [[ -f "$SECRET" ]]; then
    cp -a "$SECRET" "$SECRET.bak.$STAMP"
    echo "backed up secret -> $SECRET.bak.$STAMP"
  fi
  install -m 600 "$src" "$SECRET"
  echo "installed -> $SECRET"
}

do_reauth() {
  if [[ ! -x "$PY" ]]; then
    echo "ERROR: python missing: $PY" >&2
    exit 1
  fi
  if [[ ! -f "$SECRET" ]]; then
    echo "ERROR: missing $SECRET — install Desktop client JSON first" >&2
    exit 1
  fi
  validate_secret "$SECRET"
  if [[ -f "$TOKEN" ]]; then
    cp -a "$TOKEN" "$TOKEN.bak.$STAMP"
    echo "backed up token -> $TOKEN.bak.$STAMP"
  fi
  echo "Starting OAuth on 127.0.0.1:$PORT (open_browser=False)."
  echo "On your PC first: ssh -L $PORT:127.0.0.1:$PORT root@<vps>"
  echo "Then open the printed Google URL in the browser (channel owner account)."
  exec "$PY" - "$SECRET" "$TOKEN" "$PORT" <<'PY'
import sys
from pathlib import Path
from google_auth_oauthlib.flow import InstalledAppFlow

secret, token_path, port = Path(sys.argv[1]), Path(sys.argv[2]), int(sys.argv[3])
SCOPES = [
    "https://www.googleapis.com/auth/youtube.force-ssl",
    "https://www.googleapis.com/auth/youtube.readonly",
]
flow = InstalledAppFlow.from_client_secrets_file(str(secret), SCOPES)
creds = flow.run_local_server(
    host="127.0.0.1",
    port=port,
    open_browser=False,
    prompt="consent",
    authorization_prompt_message="Open this URL in your browser:\n{url}\n",
    success_message="OAuth OK — можно закрыть вкладку и вернуться в чат.",
)
token_path.write_text(creds.to_json())
token_path.chmod(0o600)
print(f"wrote {token_path}")
print("scopes:", sorted(creds.scopes or []))
PY
}

cmd="${1:-}"
case "$cmd" in
  install)
    do_install "${2:?path to JSON required}"
    ;;
  install-inbox)
    do_install "$INBOX_DEFAULT"
    ;;
  reauth)
    do_reauth
    ;;
  all)
    do_install "${2:?path to JSON required}"
    do_reauth
    ;;
  all-inbox)
    do_install "$INBOX_DEFAULT"
    do_reauth
    ;;
  *)
    usage
    exit 1
    ;;
esac
