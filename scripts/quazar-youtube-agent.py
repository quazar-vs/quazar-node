#!/var/lib/quazar/venv-youtube/bin/python3
"""Quazar YouTube agent — maximum channel access for Secretary orchestration.

Hands only: poll YouTube, report events, post replies. Secretary decides text.

Commands:
  --cmd status|comments|watchlist|sync-likes|pending|reply|scopes|help
  --cmd reply --parent <topLevelCommentId> --text "..."
  natural language for status/comments/pending

Env:
  HERMES_HOME / QUAZAR_YOUTUBE_HOME = /var/lib/quazar/youtube
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

HOME = Path(
    os.environ.get(
        "HERMES_HOME",
        os.environ.get("QUAZAR_YOUTUBE_HOME", "/var/lib/quazar/youtube"),
    )
)
RUN = Path(os.environ.get("QUAZAR_YOUTUBE_RUN", "/usr/local/bin/quazar-youtube-run"))
TOKEN = HOME / "google_token.json"
STATE = HOME / "youtube_secretary_bridge_state.json"
INBOX = HOME / "inbox"
MAX_VIDEOS = int(os.environ.get("QUAZAR_YT_MAX_VIDEOS", "30"))
MAX_PENDING = int(os.environ.get("QUAZAR_YT_MAX_PENDING", "10"))

# Maximum useful YouTube Data API scopes for channel ops (force-ssl covers comments).
WANTED_SCOPES = [
    "https://www.googleapis.com/auth/youtube.force-ssl",
    "https://www.googleapis.com/auth/youtube.readonly",
    "https://www.googleapis.com/auth/youtubepartner",
]


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def out(obj: dict) -> None:
    print(json.dumps(obj, ensure_ascii=False))


def yt_build():
    from google.oauth2.credentials import Credentials
    from google.auth.transport.requests import Request
    from googleapiclient.discovery import build

    if not TOKEN.exists():
        raise RuntimeError(f"NOT_AUTHENTICATED token missing: {TOKEN}")
    creds = Credentials.from_authorized_user_file(str(TOKEN))
    if creds.expired and creds.refresh_token:
        creds.refresh(Request())
        TOKEN.write_text(creds.to_json())
    return build("youtube", "v3", credentials=creds, cache_discovery=False), creds


def load_state() -> dict:
    if STATE.exists():
        try:
            return json.loads(STATE.read_text(encoding="utf-8"))
        except Exception:
            pass
    return {
        "version": 1,
        "bootstrappedAt": None,
        "seenCommentIds": [],
        "repliedTo": {},
    }


def save_state(st: dict) -> None:
    st["updatedAt"] = now_iso()
    seen = st.get("seenCommentIds") or []
    if len(seen) > 8000:
        st["seenCommentIds"] = seen[-8000:]
    STATE.parent.mkdir(parents=True, exist_ok=True)
    STATE.write_text(json.dumps(st, ensure_ascii=False, indent=2), encoding="utf-8")


def write_inbox(event: dict) -> str:
    INBOX.mkdir(parents=True, exist_ok=True)
    eid = event.get("commentId") or event.get("id") or str(int(time.time()))
    path = INBOX / f"{now_iso().replace(':', '')}_{eid}.json"
    event = dict(event)
    event["notifiedAt"] = now_iso()
    path.write_text(json.dumps(event, ensure_ascii=False, indent=2), encoding="utf-8")
    return str(path)


def run_helper(script: str, args: list[str] | None = None, timeout: int = 180) -> dict:
    args = args or []
    env = os.environ.copy()
    env["HERMES_HOME"] = str(HOME)
    env["QUAZAR_YOUTUBE_HOME"] = str(HOME)
    try:
        r = subprocess.run(
            [str(RUN), script, *args],
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
        return {"ok": False, "error": err[:400] or f"empty rc={r.returncode}", "cmd": script}
    try:
        data = json.loads(raw)
    except Exception:
        try:
            data = json.loads(raw[raw.rfind("{") :])
        except Exception:
            return {"ok": False, "error": "non-json", "raw": raw[:500], "cmd": script}
    if isinstance(data, dict):
        data.setdefault("cmd", script)
        return data
    return {"ok": True, "cmd": script, "data": data}


def already_replied_by_us(yt, parent_id: str, my_channel_id: str) -> str | None:
    from googleapiclient.errors import HttpError

    page = None
    try:
        while True:
            resp = yt.comments().list(
                part="snippet",
                parentId=parent_id,
                maxResults=100,
                textFormat="plainText",
                pageToken=page,
            ).execute()
            for it in resp.get("items") or []:
                sn = it.get("snippet") or {}
                if (sn.get("authorChannelId") or {}).get("value") == my_channel_id:
                    return it.get("id")
            page = resp.get("nextPageToken")
            if not page:
                break
    except HttpError:
        return None
    return None


def list_upload_videos(yt) -> list[str]:
    me = yt.channels().list(part="contentDetails", mine=True).execute()["items"][0]
    uploads = me["contentDetails"]["relatedPlaylists"]["uploads"]
    vids, page = [], None
    while len(vids) < MAX_VIDEOS:
        pl = yt.playlistItems().list(
            part="contentDetails",
            playlistId=uploads,
            maxResults=min(50, MAX_VIDEOS - len(vids)),
            pageToken=page,
        ).execute()
        for it in pl.get("items") or []:
            vids.append(it["contentDetails"]["videoId"])
        page = pl.get("nextPageToken")
        if not page:
            break
    return vids


def collect_pending(yt, my_id: str, st: dict) -> list[dict]:
    from googleapiclient.errors import HttpError

    seen = set(st.get("seenCommentIds") or [])
    replied = dict(st.get("repliedTo") or {})
    pending: list[dict] = []
    vids = list_upload_videos(yt)

    for vid in vids:
        page = None
        try:
            while True:
                thr = yt.commentThreads().list(
                    part="snippet,replies",
                    videoId=vid,
                    maxResults=100,
                    textFormat="plainText",
                    order="time",
                    pageToken=page,
                ).execute()
                for t in thr.get("items") or []:
                    top = t["snippet"]["topLevelComment"]
                    sn = top["snippet"]
                    top_id = top["id"]
                    aid = (sn.get("authorChannelId") or {}).get("value")
                    # foreign top-level
                    if aid and aid != my_id:
                        if top_id not in replied and top_id not in seen:
                            if already_replied_by_us(yt, top_id, my_id):
                                rid = already_replied_by_us(yt, top_id, my_id)
                                replied[top_id] = {
                                    "replyId": rid,
                                    "at": now_iso(),
                                    "videoId": vid,
                                    "source": "detected",
                                }
                                seen.add(top_id)
                            else:
                                pending.append(
                                    {
                                        "kind": "top",
                                        "commentId": top_id,
                                        "parentIdForInsert": top_id,
                                        "videoId": vid,
                                        "author": sn.get("authorDisplayName"),
                                        "authorChannelId": aid,
                                        "text": sn.get("textOriginal")
                                        or sn.get("textDisplay"),
                                        "publishedAt": sn.get("publishedAt"),
                                        "event": "YoutubeCommentPosted",
                                    }
                                )
                    # replies in thread (comment on comment)
                    for r in (t.get("replies") or {}).get("comments") or []:
                        rs = r.get("snippet") or {}
                        rid = r.get("id")
                        raid = (rs.get("authorChannelId") or {}).get("value")
                        if not rid or not raid or raid == my_id:
                            continue
                        if rid in seen or rid in replied:
                            continue
                        # need reply in same thread under top
                        pending.append(
                            {
                                "kind": "thread_reply",
                                "commentId": rid,
                                "parentIdForInsert": top_id,
                                "videoId": vid,
                                "author": rs.get("authorDisplayName"),
                                "authorChannelId": raid,
                                "text": rs.get("textOriginal") or rs.get("textDisplay"),
                                "publishedAt": rs.get("publishedAt"),
                                "event": "YoutubeCommentReplied",
                            }
                        )
                page = thr.get("nextPageToken")
                if not page:
                    break
        except HttpError as e:
            pending.append({"error": str(e)[:200], "videoId": vid})

    st["seenCommentIds"] = sorted(seen)
    st["repliedTo"] = replied
    pending = [p for p in pending if p.get("commentId")]
    pending.sort(key=lambda x: x.get("publishedAt") or "")
    return pending[:MAX_PENDING]


def cmd_scopes() -> dict:
    try:
        _yt, creds = yt_build()
        have = sorted(set(creds.scopes or []))
        missing = [s for s in WANTED_SCOPES if s not in have]
        return {
            "ok": True,
            "cmd": "scopes",
            "token_path": str(TOKEN),
            "scopes": have,
            "wanted": WANTED_SCOPES,
            "missing": missing,
            "note": (
                "youtube.force-ssl достаточно для чтения/ответов на комментарии. "
                "Полный scope youtube обычно не нужен."
            ),
            "max_access": not missing
            or "https://www.googleapis.com/auth/youtube.force-ssl" in have,
        }
    except Exception as e:
        return {"ok": False, "cmd": "scopes", "error": str(e)[:300]}


def cmd_pending(bootstrap: bool = False) -> dict:
    try:
        yt, _creds = yt_build()
        me = yt.channels().list(part="snippet", mine=True).execute()["items"][0]
        my_id = me["id"]
        st = load_state()
        if bootstrap or not st.get("bootstrappedAt"):
            # mark current tops/replies as seen, no notify
            vids = list_upload_videos(yt)
            seen = set(st.get("seenCommentIds") or [])
            for vid in vids:
                page = None
                while True:
                    thr = yt.commentThreads().list(
                        part="snippet,replies",
                        videoId=vid,
                        maxResults=100,
                        order="time",
                        pageToken=page,
                    ).execute()
                    for t in thr.get("items") or []:
                        top = t["snippet"]["topLevelComment"]
                        seen.add(top["id"])
                        for r in (t.get("replies") or {}).get("comments") or []:
                            if r.get("id"):
                                seen.add(r["id"])
                    page = thr.get("nextPageToken")
                    if not page:
                        break
            st["seenCommentIds"] = sorted(seen)
            st["bootstrappedAt"] = now_iso()
            save_state(st)
            return {
                "ok": True,
                "cmd": "pending",
                "action": "bootstrap",
                "seen": len(seen),
                "pending": [],
                "channel": me["snippet"]["title"],
                "channelId": my_id,
            }

        pending = collect_pending(yt, my_id, st)
        inbox_paths = []
        for p in pending:
            # mark seen so we don't re-emit; secretary will reply via --cmd reply
            seen = set(st.get("seenCommentIds") or [])
            seen.add(p["commentId"])
            st["seenCommentIds"] = sorted(seen)
            inbox_paths.append(write_inbox(p))
        save_state(st)
        return {
            "ok": True,
            "cmd": "pending",
            "channel": me["snippet"]["title"],
            "channelId": my_id,
            "pending": pending,
            "count": len(pending),
            "inbox": inbox_paths,
            "reply": (
                f"Агент YouTube: новых событий для Секретаря — **{len(pending)}**."
                if pending
                else "Агент YouTube: новых комментариев нет."
            ),
        }
    except Exception as e:
        return {"ok": False, "cmd": "pending", "error": str(e)[:400]}


def cmd_reply(parent_id: str, text: str, source_comment_id: str | None = None) -> dict:
    parent_id = (parent_id or "").strip()
    text = (text or "").strip()
    if not parent_id or not text:
        return {"ok": False, "cmd": "reply", "error": "parent and text required"}
    try:
        yt, _ = yt_build()
        body = {"snippet": {"parentId": parent_id, "textOriginal": text[:9500]}}
        rid = yt.comments().insert(part="snippet", body=body).execute()["id"]
        st = load_state()
        key = source_comment_id or parent_id
        replied = dict(st.get("repliedTo") or {})
        replied[key] = {
            "replyId": rid,
            "at": now_iso(),
            "parentIdForInsert": parent_id,
            "source": "secretary",
        }
        seen = set(st.get("seenCommentIds") or [])
        seen.add(key)
        seen.add(parent_id)
        st["repliedTo"] = replied
        st["seenCommentIds"] = sorted(seen)
        save_state(st)
        return {
            "ok": True,
            "cmd": "reply",
            "replyId": rid,
            "parentId": parent_id,
            "reply": f"Агент YouTube опубликовал ответ на канале (replyId=`{rid}`).",
        }
    except Exception as e:
        return {"ok": False, "cmd": "reply", "error": str(e)[:400]}


def detect_cmd(text: str) -> str:
    low = (text or "").lower().replace("ё", "е")
    if any(k in low for k in ("pending", "нов", "событ", "коммент ждут", "что нового")):
        return "pending"
    if any(k in low for k in ("scope", "доступ", "oauth", "права")):
        return "scopes"
    if any(k in low for k in ("коммент", "comment")):
        return "comments"
    if any(k in low for k in ("watchlist", "лайк", "like")):
        return "watchlist"
    if any(k in low for k in ("статус", "канал", "подпис", "ютуб", "youtube")):
        return "status"
    return "help"


def format_reply(cmd: str, data: dict) -> str:
    if data.get("reply"):
        return str(data["reply"])
    if not data.get("ok") and data.get("error"):
        return f"Агент YouTube ошибка (`{cmd}`): {data['error']}"
    if cmd == "scopes":
        return (
            f"**Доступ YouTube-агента:** scopes={data.get('scopes')}; "
            f"missing={data.get('missing')}; max_access={data.get('max_access')}. "
            f"{data.get('note')}"
        )
    if cmd == "help":
        return (
            "Агент YouTube: status, comments, pending (события для Секретаря), "
            "reply (публикация ответа), scopes, watchlist, sync-likes."
        )
    return json.dumps(data, ensure_ascii=False)[:1200]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("text", nargs="*")
    ap.add_argument(
        "--cmd",
        choices=[
            "status",
            "comments",
            "watchlist",
            "sync-likes",
            "pending",
            "reply",
            "scopes",
            "help",
            "auto-reply-dry",
        ],
    )
    ap.add_argument("--parent", default="")
    ap.add_argument("--text", dest="reply_text", default="")
    ap.add_argument("--source-comment", default="")
    ap.add_argument("--bootstrap", action="store_true")
    ap.add_argument("--from", dest="citizen", default="")
    args = ap.parse_args()
    text = " ".join(args.text).strip()
    cmd = args.cmd or (detect_cmd(text) if text else "help")

    if cmd == "help":
        data = {"ok": True, "cmd": "help"}
    elif cmd == "scopes":
        data = cmd_scopes()
    elif cmd == "pending":
        data = cmd_pending(bootstrap=args.bootstrap)
    elif cmd == "reply":
        data = cmd_reply(args.parent, args.reply_text, args.source_comment or None)
    elif cmd == "status":
        data = run_helper("quazar-youtube-status.py")
    elif cmd == "comments":
        data = run_helper("quazar-youtube-my-comments.py", timeout=300)
    elif cmd == "watchlist":
        data = run_helper("quazar-youtube-watchlist.py")
    elif cmd == "sync-likes":
        data = run_helper("quazar-youtube-sync-likes.py", timeout=300)
    elif cmd == "auto-reply-dry":
        data = run_helper("quazar-youtube-auto-reply.py", ["--dry-run"], timeout=300)
    else:
        data = {"ok": False, "error": f"unknown cmd {cmd}"}

    reply = format_reply(cmd, data if isinstance(data, dict) else {"ok": False})
    result = {
        "ok": bool(data.get("ok")) if isinstance(data, dict) else False,
        "agent": "youtube",
        "cmd": cmd,
        "citizen": args.citizen or None,
        "reply": reply,
        "data": data,
    }
    if isinstance(data, dict) and data.get("error"):
        result["ok"] = False
        result["error"] = data["error"]
    out(result)
    return 0 if result.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())
