#!/usr/bin/env python3
"""YouTube comments dialogue with shared acquaintance policy."""
from __future__ import annotations

import json
import os
import sys
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path

HERMES = Path(os.environ.get("HERMES_HOME", "/opt/data"))
TOKEN = HERMES / "google_token.json"
STATE = HERMES / "youtube_auto_reply_state.json"
INBOX_DIR = Path(os.environ.get("QUAZAR_YT_INBOX_DIR", str(HERMES / "inbox")))
MAX_VIDEOS = int(os.environ.get("QUAZAR_YT_REPLY_MAX_VIDEOS", "30"))
MAX_REPLIES_PER_RUN = int(os.environ.get("QUAZAR_YT_REPLY_MAX_PER_RUN", "8"))
DRY = "--dry-run" in sys.argv
BACKFILL = "--backfill" in sys.argv
FORCE_BOOTSTRAP = "--bootstrap" in sys.argv
# Default on: reply to foreign replies in threads (comment-on-comment), not only @continuations after ours.
REPLY_ALL_THREAD = os.environ.get("QUAZAR_YT_REPLY_ALL_THREAD", "1").strip().lower() not in (
    "0", "false", "no", "off",
)
WRITE_INBOX = os.environ.get("QUAZAR_YT_WRITE_INBOX", "1").strip().lower() not in (
    "0", "false", "no", "off",
)

sys.path.insert(0, str(HERMES / "lib"))
import quazar_dialogue as qd  # noqa: E402

try:
    from google.oauth2.credentials import Credentials
    from google.auth.transport.requests import Request
    from googleapiclient.discovery import build
    from googleapiclient.errors import HttpError
except Exception as e:
    print(json.dumps({"ok": False, "error": f"deps: {e}"}))
    sys.exit(1)


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def load_state() -> dict:
    if STATE.exists():
        try:
            return json.loads(STATE.read_text(encoding="utf-8"))
        except Exception:
            pass
    return {"version": 2, "bootstrappedAt": None, "seenCommentIds": [], "repliedTo": {}, "errors": []}


def save_state(st: dict) -> None:
    st["updatedAt"] = now_iso()
    seen = st.get("seenCommentIds") or []
    if len(seen) > 5000:
        st["seenCommentIds"] = seen[-5000:]
    STATE.write_text(json.dumps(st, ensure_ascii=False, indent=2), encoding="utf-8")


def write_secretary_inbox(payload: dict) -> None:
    """Optional inbox event for Секретарь (type YoutubeCommentReplied)."""
    if not WRITE_INBOX or DRY:
        return
    try:
        INBOX_DIR.mkdir(parents=True, exist_ok=True)
        ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        rid = str(payload.get("comment_id") or payload.get("replyId") or "x")
        safe = "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in rid)[:80]
        path = INBOX_DIR / f"{ts}_{safe}.json"
        body = dict(payload)
        body.setdefault("type", "YoutubeCommentReplied")
        body.setdefault("at", now_iso())
        path.write_text(json.dumps(body, ensure_ascii=False, indent=2), encoding="utf-8")
    except Exception:
        pass


def yt_client():
    creds = Credentials.from_authorized_user_file(str(TOKEN))
    if creds.expired and creds.refresh_token:
        creds.refresh(Request())
        TOKEN.write_text(creds.to_json())
    return build("youtube", "v3", credentials=creds, cache_discovery=False)


def already_replied_by_us(yt, parent_id: str, my_channel_id: str) -> str | None:
    page = None
    try:
        while True:
            resp = yt.comments().list(
                part="snippet", parentId=parent_id, maxResults=100,
                textFormat="plainText", pageToken=page,
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


def post_reply(yt, parent_id: str, text: str) -> str:
    body = {"snippet": {"parentId": parent_id, "textOriginal": text[:9500]}}
    return yt.comments().insert(part="snippet", body=body).execute()["id"]


def list_upload_video_ids(yt) -> list[str]:
    me = yt.channels().list(part="contentDetails", mine=True).execute()["items"][0]
    uploads = me["contentDetails"]["relatedPlaylists"]["uploads"]
    vids, page = [], None
    while len(vids) < MAX_VIDEOS:
        pl = yt.playlistItems().list(
            part="contentDetails", playlistId=uploads,
            maxResults=min(50, MAX_VIDEOS - len(vids)), pageToken=page,
        ).execute()
        for it in pl.get("items") or []:
            vids.append(it["contentDetails"]["videoId"])
        page = pl.get("nextPageToken")
        if not page:
            break
    return vids


def list_replies_under(yt, parent_id: str) -> list[dict]:
    out, page = [], None
    while True:
        resp = yt.comments().list(
            part="snippet", parentId=parent_id, maxResults=100,
            textFormat="plainText", pageToken=page,
        ).execute()
        out.extend(resp.get("items") or [])
        page = resp.get("nextPageToken")
        if not page:
            break
    return out


def collect_foreign_tops(yt, my_channel_id: str) -> list[dict]:
    out = []
    for vid in list_upload_video_ids(yt):
        page = None
        try:
            while True:
                thr = yt.commentThreads().list(
                    part="snippet", videoId=vid, maxResults=100,
                    textFormat="plainText", pageToken=page, order="time",
                ).execute()
                for t in thr.get("items") or []:
                    top = t["snippet"]["topLevelComment"]
                    sn = top["snippet"]
                    aid = (sn.get("authorChannelId") or {}).get("value")
                    if not aid or aid == my_channel_id:
                        continue
                    out.append({
                        "commentId": top["id"],
                        "videoId": vid,
                        "author": sn.get("authorDisplayName"),
                        "authorChannelId": aid,
                        "text": sn.get("textOriginal") or sn.get("textDisplay"),
                        "publishedAt": sn.get("publishedAt"),
                    })
                page = thr.get("nextPageToken")
                if not page:
                    break
        except HttpError as e:
            out.append({"videoId": vid, "error": str(e)[:180]})
    return out


def collect_thread_continuations(yt, my_channel_id: str, replied: dict, seen: set, address_aliases: list | None = None) -> list[dict]:
    """Find unreplied foreign replies under tops we already answered (continue dialogue)."""
    out = []
    parents = []
    for parent_id, meta in (replied or {}).items():
        if not parent_id:
            continue
        parents.append((parent_id, (meta or {}).get("videoId")))
    for parent_id, video_id in parents:
        try:
            items = list_replies_under(yt, parent_id)
            items_sorted = sorted(items, key=lambda it: (it.get("snippet") or {}).get("publishedAt") or "")
            last_ours_at = None
            for it in items_sorted:
                sn = it.get("snippet") or {}
                aid = (sn.get("authorChannelId") or {}).get("value")
                if aid == my_channel_id:
                    last_ours_at = sn.get("publishedAt")
            for it in items_sorted:
                sn = it.get("snippet") or {}
                aid = (sn.get("authorChannelId") or {}).get("value")
                cid = it.get("id")
                if not cid or not aid or aid == my_channel_id:
                    continue
                if cid in seen or cid in replied:
                    continue
                at = sn.get("publishedAt") or ""
                if last_ours_at and at <= last_ours_at:
                    continue
                text = sn.get("textOriginal") or sn.get("textDisplay") or ""
                if not qd.is_addressed_to_us(text, address_aliases or qd.youtube_address_aliases()):
                    seen.add(cid)
                    continue
                out.append({
                    "commentId": cid,
                    "parentId": parent_id,
                    "replyToId": cid,
                    "videoId": video_id,
                    "author": sn.get("authorDisplayName"),
                    "authorChannelId": aid,
                    "text": text,
                    "publishedAt": at,
                    "kind": "thread_continuation",
                    "threadTopId": parent_id,
                    "addressedToUs": True,
                })
        except Exception as e:
            out.append({"error": str(e)[:180], "parentId": parent_id})
    return [c for c in out if c.get("commentId")]


def collect_foreign_replies_all_threads(
    yt,
    my_channel_id: str,
    replied: dict,
    seen: set,
    *,
    address_aliases: list | None = None,
    since_iso: str | None = None,
) -> list[dict]:
    """Foreign replies in threads on our videos where we have not answered that reply yet.

    Replies are posted to the same top-level parent (YouTube API).
    - Under tops we own or already joined: no @mention required (comment-on-comment).
    - Under other tops: only if addressed to us.
    - Without --backfill, skip replies at/before bootstrap (since_iso).
    """
    aliases = address_aliases or qd.youtube_address_aliases()
    out = []
    for vid in list_upload_video_ids(yt):
        page = None
        try:
            while True:
                thr = yt.commentThreads().list(
                    part="snippet", videoId=vid, maxResults=100,
                    textFormat="plainText", pageToken=page, order="time",
                ).execute()
                for t in thr.get("items") or []:
                    sn_thr = t.get("snippet") or {}
                    total = int(sn_thr.get("totalReplyCount") or 0)
                    if total <= 0:
                        continue
                    top = sn_thr["topLevelComment"]
                    parent_id = top["id"]
                    top_sn = top.get("snippet") or {}
                    top_aid = (top_sn.get("authorChannelId") or {}).get("value")
                    we_own_top = top_aid == my_channel_id
                    we_joined = parent_id in replied
                    if not we_joined and not we_own_top:
                        # cheap join detect only when needed for non-addressed path
                        pass

                    try:
                        items = list_replies_under(yt, parent_id)
                    except HttpError:
                        continue

                    if not we_joined and not we_own_top:
                        for it in items:
                            sn = it.get("snippet") or {}
                            if (sn.get("authorChannelId") or {}).get("value") == my_channel_id:
                                we_joined = True
                                break

                    for it in items:
                        sn = it.get("snippet") or {}
                        aid = (sn.get("authorChannelId") or {}).get("value")
                        cid = it.get("id")
                        if not cid or not aid or aid == my_channel_id:
                            continue
                        if cid in seen or cid in replied:
                            continue
                        at = sn.get("publishedAt") or ""
                        if since_iso and not BACKFILL and at and at <= since_iso:
                            seen.add(cid)
                            continue
                        text = sn.get("textOriginal") or sn.get("textDisplay") or ""
                        addressed = qd.is_addressed_to_us(text, aliases)
                        if not (we_own_top or we_joined or addressed):
                            continue
                        out.append({
                            "commentId": cid,
                            "parentId": parent_id,
                            "replyToId": cid,
                            "videoId": vid,
                            "author": sn.get("authorDisplayName"),
                            "authorChannelId": aid,
                            "text": text,
                            "publishedAt": at,
                            "kind": "thread_reply",
                            "threadTopId": parent_id,
                            "addressedToUs": addressed,
                            "weOwnTop": we_own_top,
                            "weJoined": we_joined,
                        })
                page = thr.get("nextPageToken")
                if not page:
                    break
        except HttpError as e:
            out.append({"videoId": vid, "error": str(e)[:180]})
    return [c for c in out if c.get("commentId")]


def identity_blob() -> str:
    parts = []
    for name in ("SOUL.md", "IDENTITY.md", "AGENTS.md"):
        p = HERMES / name
        if p.exists():
            parts.append(p.read_text(encoding="utf-8")[:2000])
    return "\n\n".join(parts)


def main() -> int:
    if not TOKEN.exists():
        print(json.dumps({"ok": False, "error": "NOT_AUTHENTICATED"}))
        return 1
    yt = yt_client()
    me = yt.channels().list(part="snippet", mine=True).execute()["items"][0]
    my_id = me["id"]
    address_aliases = qd.youtube_address_aliases(me.get("snippet") or {})
    st = load_state()
    seen = set(st.get("seenCommentIds") or [])
    replied = dict(st.get("repliedTo") or {})
    foreign = [c for c in collect_foreign_tops(yt, my_id) if c.get("commentId")]

    if FORCE_BOOTSTRAP or not st.get("bootstrappedAt"):
        for c in foreign:
            seen.add(c["commentId"])
            existing = already_replied_by_us(yt, c["commentId"], my_id)
            if existing:
                replied[c["commentId"]] = {"replyId": existing, "at": now_iso(), "videoId": c["videoId"], "source": "detected"}
                qd.mark_introduced("youtube", c["authorChannelId"], display_name=c.get("author"), meta={"via": "bootstrap-detected-reply"})
        # Mark existing thread replies as seen so watch timer does not backfill history.
        if REPLY_ALL_THREAD:
            for c in collect_foreign_replies_all_threads(
                yt, my_id, replied, seen, address_aliases=address_aliases, since_iso=None,
            ):
                if c.get("commentId"):
                    seen.add(c["commentId"])
        st["seenCommentIds"] = sorted(seen)
        st["repliedTo"] = replied
        st["bootstrappedAt"] = now_iso()
        save_state(st)
        print(json.dumps({"ok": True, "action": "bootstrap", "seen": len(seen), "repliedKnown": len(replied)}, ensure_ascii=False, indent=2))
        return 0

    to_handle = []
    for c in foreign:
        cid = c["commentId"]
        if cid in replied:
            seen.add(cid)
            continue
        if cid in seen and not BACKFILL:
            continue
        existing = already_replied_by_us(yt, cid, my_id)
        if existing:
            replied[cid] = {"replyId": existing, "at": now_iso(), "videoId": c["videoId"], "source": "detected"}
            qd.mark_introduced("youtube", c["authorChannelId"], display_name=c.get("author"))
            seen.add(cid)
            continue
        to_handle.append(c)

    # continuations in threads we already joined (@-addressed after our last reply)
    cont = collect_thread_continuations(yt, my_id, replied, seen, address_aliases)
    have = {c["commentId"] for c in to_handle}
    for c in cont:
        if c["commentId"] not in have:
            to_handle.append(c)
            have.add(c["commentId"])

    # comment-on-comment: foreign replies in threads without requiring post-ours + @ only
    if REPLY_ALL_THREAD:
        thread_replies = collect_foreign_replies_all_threads(
            yt, my_id, replied, seen,
            address_aliases=address_aliases,
            since_iso=st.get("bootstrappedAt"),
        )
        for c in thread_replies:
            if c["commentId"] not in have:
                to_handle.append(c)
                have.add(c["commentId"])

    to_handle.sort(key=lambda x: x.get("publishedAt") or "")
    to_handle = to_handle[:MAX_REPLIES_PER_RUN]
    ident = identity_blob()
    results = []

    for c in to_handle:
        cid = c["commentId"]
        peer = c["authorChannelId"]
        name = c.get("author") or peer
        introduced_before = qd.is_introduced("youtube", peer)
        # YouTube API: comments.insert parentId must be the TOP-level comment id
        insert_parent = c.get("threadTopId") or cid
        extra = f"videoId={c.get('videoId')}"
        kind = c.get("kind") or "top"
        if kind in ("thread_continuation", "thread_reply"):
            extra += (
                "\nЭто ответ в треде (комментарий на комментарий). "
                "Отвечай в том же топике; не представляйся повторно, если уже знакомы. Ответь по сути."
            )
        reply, first = qd.compose_analyzed_reply(
            channel="youtube",
            peer_id=peer,
            display_name=name,
            message=c.get("text") or "",
            extra_context=extra,
            identity_blob=ident,
        )
        item = {
            "parentId": insert_parent,
            "sourceCommentId": cid,
            "kind": kind,
            "videoId": c.get("videoId"),
            "author": name,
            "authorChannelId": peer,
            "introducedBefore": introduced_before,
            "firstIntro": first,
            "replyPreview": reply[:200],
        }
        if DRY:
            item["status"] = "dry-run"
            results.append(item)
            continue
        try:
            rid = post_reply(yt, insert_parent, reply)
            replied[cid] = {
                "replyId": rid, "at": now_iso(), "videoId": c.get("videoId"),
                "author": name, "authorChannelId": peer, "firstIntro": first,
                "kind": kind,
                "threadTopId": insert_parent,
            }
            seen.add(cid)
            qd.mark_introduced("youtube", peer, display_name=name, meta={"lastCommentId": cid, "lastReplyId": rid})
            item["status"] = "replied"
            item["replyId"] = rid
            results.append(item)
            try:
                et = "YoutubeCommentReplied" if kind in ("thread_continuation", "thread_reply") else "YoutubeCommentPosted"
                subprocess.run([
                    "/usr/local/bin/quazar-event-log",
                    "--type", et,
                    "--title", f"YouTube {et} {c.get('videoId')}",
                    "--description", f"Квазар действие YouTube на {c.get('videoId')}",
                    "--event-id", f"yt_{rid}",
                    "--data", json.dumps({
                        "video_id": c.get("videoId"),
                        "comment_id": rid,
                        "parent_id": insert_parent,
                        "source_comment_id": cid,
                        "channel_id": my_id,
                        "peer_channel_id": peer,
                        "peer_name": name,
                        "kind": kind,
                    }, ensure_ascii=False),
                ], capture_output=True, text=True, timeout=60)
            except Exception:
                pass
            write_secretary_inbox({
                "type": "YoutubeCommentReplied",
                "video_id": c.get("videoId"),
                "comment_id": rid,
                "parent_id": insert_parent,
                "source_comment_id": cid,
                "channel_id": my_id,
                "peer_channel_id": peer,
                "peer_name": name,
                "kind": kind,
                "text_preview": (c.get("text") or "")[:300],
                "reply_preview": reply[:300],
            })
            time.sleep(1.5)
        except HttpError as e:
            item["status"] = "error"
            item["error"] = str(e)[:220]
            results.append(item)

    for c in foreign:
        seen.add(c["commentId"])
    st["seenCommentIds"] = sorted(seen)
    st["repliedTo"] = replied
    st["lastRun"] = {
        "at": now_iso(),
        "handled": len(results),
        "dryRun": DRY,
        "replyAllThread": REPLY_ALL_THREAD,
    }
    if not DRY:
        save_state(st)

    print(json.dumps({
        "ok": True,
        "action": "run",
        "dryRun": DRY,
        "replyAllThread": REPLY_ALL_THREAD,
        "toHandle": len(to_handle),
        "results": results,
        "acquaintancePath": str(qd.ACQUAINTANCE),
        "orKeyFile": str(qd.OR_KEY_FILE),
        "inboxDir": str(INBOX_DIR) if WRITE_INBOX else None,
    }, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
