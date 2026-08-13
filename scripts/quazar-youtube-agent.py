#!/usr/bin/env python3
"""Quazar YouTube agent — bridge for Secretary ↔ YouTube helpers.

Usage:
  quazar-youtube-agent "статус канала"
  quazar-youtube-agent --cmd status
  quazar-youtube-agent --cmd comments
  quazar-youtube-agent --json "сколько подписчиков"

Speaks JSON on stdout (last line = machine result). Human summary in field `reply`.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HOME = Path(os.environ.get("HERMES_HOME", os.environ.get("QUAZAR_YOUTUBE_HOME", "/var/lib/quazar/youtube")))
RUN = Path(os.environ.get("QUAZAR_YOUTUBE_RUN", "/usr/local/bin/quazar-youtube-run"))
PY_BIN = Path(os.environ.get("QUAZAR_YOUTUBE_PY", "/var/lib/quazar/venv-youtube/bin/python3"))


def out(obj: dict) -> None:
    print(json.dumps(obj, ensure_ascii=False))


def run_helper(script: str, args: list[str] | None = None, timeout: int = 180) -> dict:
    args = args or []
    env = os.environ.copy()
    env["HERMES_HOME"] = str(HOME)
    env["QUAZAR_YOUTUBE_HOME"] = str(HOME)
    cmd = [str(RUN), script, *args]
    try:
        r = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
        )
    except Exception as e:
        return {"ok": False, "error": f"exec: {e}", "cmd": script}
    raw = (r.stdout or "").strip()
    err = (r.stderr or "").strip()
    if not raw:
        return {
            "ok": False,
            "error": err[:400] or f"empty stdout rc={r.returncode}",
            "cmd": script,
        }
    # helpers print JSON (sometimes pretty)
    try:
        data = json.loads(raw)
    except Exception:
        # try last JSON object
        try:
            data = json.loads(raw[raw.rfind("{") :])
        except Exception:
            return {
                "ok": False,
                "error": "non-json helper output",
                "cmd": script,
                "raw": raw[:500],
                "stderr": err[:200],
            }
    if isinstance(data, dict):
        data.setdefault("cmd", script)
        if r.returncode != 0 and "ok" not in data:
            data["ok"] = False
        return data
    return {"ok": True, "cmd": script, "data": data}


def detect_cmd(text: str) -> str:
    low = (text or "").lower().replace("ё", "е")
    if any(k in low for k in ("автоответ", "auto-reply", "auto reply", "ответь на коммент")):
        return "auto-reply-dry"
    if any(k in low for k in ("синхрон", "sync-like", "sync like", "лайк", "likes")):
        return "sync-likes"
    if any(k in low for k in ("watchlist", "вотчлист", "список видео", "видео канала")):
        return "watchlist"
    if any(k in low for k in ("коммент", "comment", "мои комментар")):
        return "comments"
    if any(
        k in low
        for k in (
            "статус",
            "канал",
            "подпис",
            "просмотр",
            "youtube",
            "ютуб",
            "you tube",
            "quazarvs",
        )
    ):
        return "status"
    if any(k in low for k in ("помощь", "help", "что умеешь", "команды")):
        return "help"
    return "status"


def format_reply(cmd: str, data: dict) -> str:
    if not data.get("ok", True) and data.get("error"):
        return f"Агент YouTube: ошибка (`{cmd}`): {data.get('error')}"

    if cmd == "help":
        return (
            "Агент YouTube умеет: статус канала, мои комментарии, watchlist, "
            "синхронизация лайков, dry-run автоответов. "
            "Спросите, например: «статус YouTube» или «мои комментарии на канале»."
        )

    if cmd == "status":
        title = data.get("title") or "?"
        cid = data.get("id") or "?"
        return (
            f"**Агент YouTube** — канал **{title}** (`{cid}`).\n"
            f"Подписчики: {data.get('subs')}; видео: {data.get('videos')}; "
            f"просмотры: {data.get('views')}."
        )

    if cmd == "comments":
        comments = data.get("comments") or []
        n = data.get("count") if data.get("count") is not None else len(comments)
        lines = [
            f"**Агент YouTube** — мои комментарии на канале "
            f"**{data.get('title') or '?'}**: найдено **{n}** "
            f"(видео просмотрено: {data.get('videosScanned')})."
        ]
        for c in comments[:5]:
            text = (c.get("text") or "").replace("\n", " ").strip()
            if len(text) > 120:
                text = text[:117] + "…"
            kind = c.get("kind") or "comment"
            lines.append(f"- [{kind}] {text}")
        if n and int(n) > 5:
            lines.append(f"… и ещё {int(n) - 5}.")
        return "\n".join(lines)

    if cmd == "watchlist":
        items = data.get("videos") or data.get("items") or data.get("watchlist") or []
        if isinstance(items, dict):
            items = items.get("items") or []
        lines = [f"**Агент YouTube** — watchlist / видео (записей: {len(items) if isinstance(items, list) else '?'})."]
        if isinstance(items, list):
            for it in items[:8]:
                if isinstance(it, dict):
                    lines.append(
                        f"- {it.get('title') or it.get('id') or it.get('videoId') or it}"
                    )
                else:
                    lines.append(f"- {it}")
        else:
            lines.append(json.dumps(data, ensure_ascii=False)[:800])
        return "\n".join(lines)

    if cmd == "sync-likes":
        return (
            f"**Агент YouTube** — sync-likes: "
            f"{json.dumps({k: data.get(k) for k in data if k != 'cmd'}, ensure_ascii=False)[:900]}"
        )

    if cmd == "auto-reply-dry":
        return (
            f"**Агент YouTube** — auto-reply (dry-run): "
            f"{json.dumps({k: data.get(k) for k in data if k != 'cmd'}, ensure_ascii=False)[:900]}"
        )

    return f"**Агент YouTube** (`{cmd}`):\n```json\n{json.dumps(data, ensure_ascii=False)[:1200]}\n```"


def execute(cmd: str) -> dict:
    if cmd == "help":
        return {"ok": True, "cmd": "help"}
    if cmd == "status":
        return run_helper("quazar-youtube-status.py")
    if cmd == "comments":
        return run_helper("quazar-youtube-my-comments.py", timeout=300)
    if cmd == "watchlist":
        return run_helper("quazar-youtube-watchlist.py", timeout=180)
    if cmd == "sync-likes":
        return run_helper("quazar-youtube-sync-likes.py", timeout=300)
    if cmd == "auto-reply-dry":
        return run_helper("quazar-youtube-auto-reply.py", ["--dry-run"], timeout=300)
    return {"ok": False, "error": f"unknown cmd: {cmd}", "cmd": cmd}


def main() -> int:
    ap = argparse.ArgumentParser(description="Quazar YouTube agent")
    ap.add_argument("text", nargs="*", help="Natural language request")
    ap.add_argument("--cmd", choices=["status", "comments", "watchlist", "sync-likes", "auto-reply-dry", "help"])
    ap.add_argument("--json", action="store_true", help="Print only JSON result")
    ap.add_argument("--from", dest="citizen", default="", help="Requester (informational)")
    args = ap.parse_args()
    text = " ".join(args.text).strip()
    cmd = args.cmd or (detect_cmd(text) if text else "help")
    data = execute(cmd)
    reply = format_reply(cmd, data)
    result = {
        "ok": bool(data.get("ok", False)) or (cmd == "help"),
        "agent": "youtube",
        "cmd": cmd,
        "citizen": args.citizen or None,
        "reply": reply,
        "data": data,
    }
    if data.get("error") and cmd != "help":
        result["ok"] = False
        result["error"] = data.get("error")
    out(result)
    return 0 if result.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())
