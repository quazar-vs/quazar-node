#!/var/lib/quazar/venv-youtube/bin/python3
"""Алгонизм YouTube (algonism-youtube) — hands for Secretary orchestration.

Binary path remains `quazar-youtube-agent` (symlink: `algonism-youtube`).
Hands only: poll YouTube, report events, post replies. Secretary decides text.

Commands:
  --cmd status|comments|watchlist|sync-likes|pending|reply|scopes|help
  --cmd reply --parent <topLevelCommentId> --text "..."
  natural language for status/comments/pending

Env:
  HERMES_HOME / QUAZAR_YOUTUBE_HOME = /var/lib/quazar/youtube
  YOUTUBE_CHANNEL_ID / QUAZAR_YOUTUBE_CHANNEL_ID = target channel (UC…)
  YOUTUBE_CHANNEL_TITLE / QUAZAR_YOUTUBE_CHANNEL_TITLE = display name (optional)
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

# Previous QuazarVS channel (suspended / legacy). Kept for docs only.
LEGACY_CHANNEL_ID = "UCiKVQ7nwemLBAuEtOlr8rmw"


def configured_channel_id() -> str:
    return (
        os.environ.get("YOUTUBE_CHANNEL_ID")
        or os.environ.get("QUAZAR_YOUTUBE_CHANNEL_ID")
        or ""
    ).strip()


def configured_channel_title() -> str:
    return (
        os.environ.get("YOUTUBE_CHANNEL_TITLE")
        or os.environ.get("QUAZAR_YOUTUBE_CHANNEL_TITLE")
        or ""
    ).strip()


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def out(obj: dict) -> None:
    print(json.dumps(obj, ensure_ascii=False))


def classify_auth_error(exc: BaseException) -> str:
    msg = str(exc)
    low = msg.lower()
    if "invalid_grant" in low or "revoked" in low:
        return (
            "OAuth refresh failed (invalid_grant / revoked). "
            "Re-login Google OAuth for the account that owns the target channel; "
            f"write token to {TOKEN}."
        )
    if "authenticateduseraccountsuspended" in low.replace(" ", "") or "suspended" in low:
        return (
            "YouTube account suspended for this OAuth identity. "
            "Use a Google account that owns a live channel (or unsuspend), "
            "then set YOUTUBE_CHANNEL_ID and re-auth."
        )
    return msg[:400]


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
        "channelId": None,
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
    cid = configured_channel_id()
    if cid:
        env.setdefault("YOUTUBE_CHANNEL_ID", cid)
        env.setdefault("QUAZAR_YOUTUBE_CHANNEL_ID", cid)
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


def resolve_channel(yt) -> dict:
    """Return {id, title, uploadsPlaylistId} for configured or mine channel."""
    from googleapiclient.errors import HttpError

    want = configured_channel_id()
    title_hint = configured_channel_title()
    if want:
        try:
            resp = yt.channels().list(
                part="snippet,contentDetails,status",
                id=want,
            ).execute()
        except HttpError as e:
            raise RuntimeError(classify_auth_error(e)) from e
        items = resp.get("items") or []
        if not items:
            raise RuntimeError(
                f"Channel {want} not visible to this OAuth token "
                "(wrong Google account, no access, or private). "
                "Re-auth as the owner of that channel."
            )
        ch = items[0]
        uploads = (
            (ch.get("contentDetails") or {})
            .get("relatedPlaylists", {})
            .get("uploads")
        )
        if not uploads:
            raise RuntimeError(f"Channel {want} has no uploads playlist (contentDetails).")
        return {
            "id": ch["id"],
            "title": (ch.get("snippet") or {}).get("title") or title_hint or want,
            "uploadsPlaylistId": uploads,
            "source": "YOUTUBE_CHANNEL_ID",
            "status": ch.get("status") or {},
        }

    try:
        resp = yt.channels().list(part="snippet,contentDetails,status", mine=True).execute()
    except HttpError as e:
        raise RuntimeError(classify_auth_error(e)) from e
    items = resp.get("items") or []
    if not items:
        raise RuntimeError(
            "No channel for mine=true. Set YOUTUBE_CHANNEL_ID to the brand channel UC… id."
        )
    ch = items[0]
    uploads = (
        (ch.get("contentDetails") or {}).get("relatedPlaylists", {}).get("uploads")
    )
    if not uploads:
        raise RuntimeError("mine channel has no uploads playlist")
    return {
        "id": ch["id"],
        "title": (ch.get("snippet") or {}).get("title") or title_hint or ch["id"],
        "uploadsPlaylistId": uploads,
        "source": "mine",
        "status": ch.get("status") or {},
    }


def already_replied_by_us(yt, parent_id: str, my_channel_id: str) -> str | None:
    from googleapiclient.errors import HttpError

    page = None
    try:
        while True:
            resp = (
                yt.comments()
                .list(
                    part="snippet",
                    parentId=parent_id,
                    maxResults=100,
                    textFormat="plainText",
                    pageToken=page,
                )
                .execute()
            )
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


def list_replies_under(yt, parent_id: str) -> list[dict]:
    out, page = [], None
    while True:
        resp = (
            yt.comments()
            .list(
                part="snippet",
                parentId=parent_id,
                maxResults=100,
                textFormat="plainText",
                pageToken=page,
            )
            .execute()
        )
        out.extend(resp.get("items") or [])
        page = resp.get("nextPageToken")
        if not page:
            break
    return out


def list_upload_videos(yt, uploads_playlist_id: str) -> list[str]:
    vids, page = [], None
    while len(vids) < MAX_VIDEOS:
        pl = (
            yt.playlistItems()
            .list(
                part="contentDetails",
                playlistId=uploads_playlist_id,
                maxResults=min(50, MAX_VIDEOS - len(vids)),
                pageToken=page,
            )
            .execute()
        )
        for it in pl.get("items") or []:
            vids.append(it["contentDetails"]["videoId"])
        page = pl.get("nextPageToken")
        if not page:
            break
    return vids


def collect_pending(yt, my_id: str, uploads_playlist_id: str, st: dict) -> list[dict]:
    """New foreign tops under our videos + foreign replies in those threads.

    Replies: full comments.list(parentId=) pagination (not only the ~5 inline
    replies from commentThreads). We surface replies in threads where we own
    the top, already joined, or any foreign reply under our videos (secretary
    decides whether to answer).
    """
    from googleapiclient.errors import HttpError

    seen = set(st.get("seenCommentIds") or [])
    replied = dict(st.get("repliedTo") or {})
    pending: list[dict] = []
    vids = list_upload_videos(yt, uploads_playlist_id)

    for vid in vids:
        page = None
        try:
            while True:
                thr = (
                    yt.commentThreads()
                    .list(
                        part="snippet",
                        videoId=vid,
                        maxResults=100,
                        textFormat="plainText",
                        order="time",
                        pageToken=page,
                    )
                    .execute()
                )
                for t in thr.get("items") or []:
                    sn_thr = t.get("snippet") or {}
                    top = sn_thr["topLevelComment"]
                    sn = top["snippet"]
                    top_id = top["id"]
                    aid = (sn.get("authorChannelId") or {}).get("value")
                    we_own_top = aid == my_id
                    total_replies = int(sn_thr.get("totalReplyCount") or 0)

                    # foreign top-level comments under our videos
                    if aid and aid != my_id:
                        if top_id not in replied and top_id not in seen:
                            ours = already_replied_by_us(yt, top_id, my_id)
                            if ours:
                                replied[top_id] = {
                                    "replyId": ours,
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

                    # replies in thread (full pagination)
                    if total_replies <= 0 and not we_own_top:
                        continue
                    try:
                        items = list_replies_under(yt, top_id)
                    except HttpError:
                        continue

                    we_joined = top_id in replied or we_own_top
                    if not we_joined:
                        for it in items:
                            rs = it.get("snippet") or {}
                            if (rs.get("authorChannelId") or {}).get("value") == my_id:
                                we_joined = True
                                break

                    for it in items:
                        rs = it.get("snippet") or {}
                        rid = it.get("id")
                        raid = (rs.get("authorChannelId") or {}).get("value")
                        if not rid or not raid or raid == my_id:
                            continue
                        if rid in seen or rid in replied:
                            continue
                        # Prefer threads we own/joined; still emit foreign replies
                        # under our videos so secretary can answer continuations.
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
                                "weJoined": we_joined,
                                "weOwnTop": we_own_top,
                            }
                        )
                page = thr.get("nextPageToken")
                if not page:
                    break
        except HttpError as e:
            pending.append(
                {
                    "error": classify_auth_error(e)[:200],
                    "videoId": vid,
                }
            )

    st["seenCommentIds"] = sorted(seen)
    st["repliedTo"] = replied
    pending = [p for p in pending if p.get("commentId")]
    pending.sort(key=lambda x: x.get("publishedAt") or "")
    return pending[:MAX_PENDING]


def maybe_reset_state_for_channel(st: dict, channel_id: str) -> bool:
    """If target channel changed, clear seen/bootstrap so we don't skip new events."""
    prev = (st.get("channelId") or "").strip()
    if prev and prev != channel_id:
        st["seenCommentIds"] = []
        st["repliedTo"] = {}
        st["bootstrappedAt"] = None
        st["channelId"] = channel_id
        st["channelSwitchedAt"] = now_iso()
        st["previousChannelId"] = prev
        return True
    st["channelId"] = channel_id
    return False


def bootstrap_seen(yt, uploads_playlist_id: str, st: dict) -> int:
    seen = set(st.get("seenCommentIds") or [])
    for vid in list_upload_videos(yt, uploads_playlist_id):
        page = None
        while True:
            thr = (
                yt.commentThreads()
                .list(
                    part="snippet",
                    videoId=vid,
                    maxResults=100,
                    order="time",
                    pageToken=page,
                )
                .execute()
            )
            for t in thr.get("items") or []:
                sn_thr = t.get("snippet") or {}
                top = sn_thr["topLevelComment"]
                top_id = top["id"]
                seen.add(top_id)
                total = int(sn_thr.get("totalReplyCount") or 0)
                if total > 0:
                    try:
                        for r in list_replies_under(yt, top_id):
                            if r.get("id"):
                                seen.add(r["id"])
                    except Exception:
                        pass
            page = thr.get("nextPageToken")
            if not page:
                break
    st["seenCommentIds"] = sorted(seen)
    st["bootstrappedAt"] = now_iso()
    return len(seen)


def cmd_scopes() -> dict:
    try:
        _yt, creds = yt_build()
        have = sorted(set(creds.scopes or []))
        missing = [s for s in WANTED_SCOPES if s not in have]
        return {
            "ok": True,
            "cmd": "scopes",
            "token_path": str(TOKEN),
            "configuredChannelId": configured_channel_id() or None,
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
        return {"ok": False, "cmd": "scopes", "error": classify_auth_error(e)}


def cmd_pending(bootstrap: bool = False) -> dict:
    try:
        yt, _creds = yt_build()
        ch = resolve_channel(yt)
        my_id = ch["id"]
        st = load_state()
        switched = maybe_reset_state_for_channel(st, my_id)
        if switched:
            save_state(st)

        if bootstrap or not st.get("bootstrappedAt") or switched:
            seen_n = bootstrap_seen(yt, ch["uploadsPlaylistId"], st)
            save_state(st)
            return {
                "ok": True,
                "cmd": "pending",
                "action": "bootstrap",
                "seen": seen_n,
                "pending": [],
                "channel": ch["title"],
                "channelId": my_id,
                "channelSource": ch["source"],
                "channelSwitched": switched,
                "reply": (
                    f"Алгонизм YouTube: bootstrap канала **{ch['title']}** "
                    f"(`{my_id}`), отмечено {seen_n} комментов/ответов."
                ),
            }

        pending = collect_pending(yt, my_id, ch["uploadsPlaylistId"], st)
        inbox_paths = []
        for p in pending:
            seen = set(st.get("seenCommentIds") or [])
            seen.add(p["commentId"])
            st["seenCommentIds"] = sorted(seen)
            inbox_paths.append(write_inbox(p))
        save_state(st)
        return {
            "ok": True,
            "cmd": "pending",
            "channel": ch["title"],
            "channelId": my_id,
            "channelSource": ch["source"],
            "pending": pending,
            "count": len(pending),
            "inbox": inbox_paths,
            "reply": (
                f"Алгонизм YouTube [{ch['title']}]: новых событий для Секретаря — "
                f"**{len(pending)}**."
                if pending
                else f"Алгонизм YouTube [{ch['title']}]: новых комментариев нет."
            ),
        }
    except Exception as e:
        return {
            "ok": False,
            "cmd": "pending",
            "error": classify_auth_error(e),
            "configuredChannelId": configured_channel_id() or None,
        }


def cmd_reply(parent_id: str, text: str, source_comment_id: str | None = None) -> dict:
    parent_id = (parent_id or "").strip()
    text = (text or "").strip()
    if not parent_id or not text:
        return {"ok": False, "cmd": "reply", "error": "parent and text required"}
    try:
        yt, _ = yt_build()
        # Ensure we resolve/configured channel is reachable (auth check).
        ch = resolve_channel(yt)
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
            "channelId": ch["id"],
        }
        seen = set(st.get("seenCommentIds") or [])
        seen.add(key)
        seen.add(parent_id)
        st["repliedTo"] = replied
        st["seenCommentIds"] = sorted(seen)
        st["channelId"] = ch["id"]
        save_state(st)
        return {
            "ok": True,
            "cmd": "reply",
            "replyId": rid,
            "parentId": parent_id,
            "channelId": ch["id"],
            "channel": ch["title"],
            "reply": (
                f"Алгонизм YouTube опубликовал ответ на канале {ch['title']} "
                f"(replyId=`{rid}`)."
            ),
        }
    except Exception as e:
        return {"ok": False, "cmd": "reply", "error": classify_auth_error(e)}


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
        return f"Алгонизм YouTube ошибка (`{cmd}`): {data['error']}"
    if cmd == "scopes":
        return (
            f"**Доступ Алгонизм YouTube:** channel={data.get('configuredChannelId')}; "
            f"scopes={data.get('scopes')}; missing={data.get('missing')}; "
            f"max_access={data.get('max_access')}. {data.get('note')}"
        )
    if cmd == "help":
        return (
            "Алгонизм YouTube (quazar-youtube-agent / algonism-youtube): "
            "status, comments, pending (события для Секретаря), "
            "reply (публикация ответа), scopes, watchlist, sync-likes. "
            f"YOUTUBE_CHANNEL_ID={configured_channel_id() or '(mine)'}."
        )
    return json.dumps(data, ensure_ascii=False)[:1200]


def main() -> int:
    ap = argparse.ArgumentParser(
        prog="algonism-youtube",
        description="Алгонизм YouTube — OAuth/pending/reply для Секретаря "
        "(binary: quazar-youtube-agent).",
    )
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
        cid = configured_channel_id()
        helper_args = [cid] if cid else []
        data = run_helper("quazar-youtube-status.py", helper_args)
        if isinstance(data, dict):
            data["configuredChannelId"] = cid or None
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
        "agent": "algonism-youtube",
        "cmd": cmd,
        "citizen": args.citizen or None,
        "reply": reply,
        "data": data,
        "configuredChannelId": configured_channel_id() or None,
    }
    if isinstance(data, dict) and data.get("error"):
        result["ok"] = False
        result["error"] = data["error"]
    out(result)
    return 0 if result.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())
