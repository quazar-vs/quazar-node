#!/bin/bash
# Hourly YouTube maintenance. Comment replies are NOT here —
# Secretary polls quazar-youtube-agent --cmd pending and posts via --cmd reply.
set -euo pipefail
LOG=/var/log/quazar-youtube-hourly.log
exec >>"$LOG" 2>&1
echo "==== $(date -u +%Y-%m-%dT%H:%M:%SZ) hourly start ===="
/usr/local/bin/quazar-youtube-sync-likes || echo "sync-likes failed: $?"
/usr/local/bin/quazar-youtube-intro-on-likes || echo "intro-on-likes failed: $?"
/usr/local/bin/quazar-youtube-intro-replies || echo "intro-replies failed: $?"
echo "==== $(date -u +%Y-%m-%dT%H:%M:%SZ) hourly done (no auto-reply; secretary bridge) ===="
