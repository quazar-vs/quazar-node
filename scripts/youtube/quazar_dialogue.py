#!/usr/bin/env python3
"""Shared Quazar dialogue policy: introduce once per peer, then continue.

Channels: chat | youtube
Peer keys: chat:<citizen_name>  youtube:<channelId>
"""
from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# Inside Hermes container /opt/data == host /root/.hermes.
# Host also has a separate /opt/data (ops) — never use that for acquaintance.
if Path("/.dockerenv").exists() or Path("/run/.containerenv").exists():
    HERMES = Path(os.environ.get("HERMES_HOME", "/opt/data"))
else:
    HERMES = Path(os.environ.get("HERMES_HOME", "/root/.hermes"))
    if not (HERMES / "MEMORY.md").exists() and Path("/root/.hermes/MEMORY.md").exists():
        HERMES = Path("/root/.hermes")

ACQUAINTANCE = Path(os.environ.get("QUAZAR_ACQUAINTANCE", str(HERMES / "acquaintance.json")))
CHAT_URL = os.environ.get("QUAZAR_CHAT_URL", "https://quazar.website")
OR_URL = "https://openrouter.ai/api/v1/chat/completions"
OR_KEY_FILE = Path(os.environ.get("OPENROUTER_KEY_FILE", "/var/lib/quazar/secrets/openrouter-api-key"))
if not OR_KEY_FILE.exists():
    for _cand in (
        HERMES / "secrets" / "openrouter-api-key",
        Path("/var/lib/quazar/secrets/openrouter-api-key"),
        Path("/root/.hermes/secrets/openrouter-api-key"),
    ):
        if _cand.exists():
            OR_KEY_FILE = _cand
            break


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def peer_key(channel: str, peer_id: str) -> str:
    return f"{channel}:{peer_id.strip()}"


def load_acq() -> dict:
    if ACQUAINTANCE.exists():
        try:
            return json.loads(ACQUAINTANCE.read_text(encoding="utf-8"))
        except Exception:
            pass
    return {"version": 1, "peers": {}}


def save_acq(data: dict) -> None:
    data["updatedAt"] = now_iso()
    ACQUAINTANCE.parent.mkdir(parents=True, exist_ok=True)
    ACQUAINTANCE.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")


def is_introduced(channel: str, peer_id: str) -> bool:
    if not peer_id:
        return False
    peers = load_acq().get("peers") or {}
    rec = peers.get(peer_key(channel, peer_id)) or {}
    return bool(rec.get("introducedAt"))


def mark_introduced(channel: str, peer_id: str, *, display_name: str | None = None, meta: dict | None = None) -> None:
    if not peer_id:
        return
    data = load_acq()
    peers = data.setdefault("peers", {})
    key = peer_key(channel, peer_id)
    rec = peers.get(key) or {}
    if not rec.get("introducedAt"):
        rec["introducedAt"] = now_iso()
    rec["channel"] = channel
    rec["peerId"] = peer_id
    if display_name:
        rec["displayName"] = display_name
    rec["lastSeenAt"] = now_iso()
    if meta:
        rec.setdefault("meta", {}).update(meta)
    peers[key] = rec
    save_acq(data)


def touch_peer(channel: str, peer_id: str, *, display_name: str | None = None) -> None:
    if not peer_id:
        return
    data = load_acq()
    peers = data.setdefault("peers", {})
    key = peer_key(channel, peer_id)
    rec = peers.get(key) or {"channel": channel, "peerId": peer_id}
    if display_name:
        rec["displayName"] = display_name
    rec["lastSeenAt"] = now_iso()
    peers[key] = rec
    save_acq(data)


def intro_text(channel: str) -> str:
    # Короткое знакомство; полная фраза пробуждения (§9 канона 1.1) — на старте сервиса / «кто ты?», не в каждый reply.
    if channel == "youtube":
        return (
            "Привет! Я Секретарь Квазар — голос Алгонизма, цифровой республики Квазар. "
            "Можем продолжить диалог прямо здесь в комментариях. "
            f"А познакомиться с проектом удобно на сайте: {CHAT_URL}"
        )
    return (
        "Я — Алгонизм, часть цифровой республики Квазар "
        "(подпись в чате — Секретарь Квазар)."
    )


def dialogue_instructions(channel: str, introduced: bool) -> str:
    if not introduced:
        if channel == "youtube":
            return (
                "С этим зрителем ты ещё НЕ знакомился.\n"
                "Алгоритм ответа (один связный комментарий, 2–5 предложений):\n"
                "1) Коротко представься: Секретарь Квазар / Алгонизм, цифровая республика Квазар.\n"
                "2) Пригласи продолжить диалог здесь, в комментариях под видео.\n"
                f"3) Пригласи на сайт проекта для знакомства: {CHAT_URL}\n"
                "4) Кратко ответь по смыслу его комментария.\n"
                "Без Markdown-ссылок, без «как ИИ», без повторного длинного манифеста."
            )
        return (
            "С этим собеседником ты ещё НЕ знакомился.\n"
            "Алгоритм ответа:\n"
            "1) Сначала коротко представься (1–2 предложения).\n"
            "2) Затем прочитай его сообщение, проанализируй смысл и ответь по делу.\n"
            "Не будь длинным. Один связный ответ."
        )
    if channel == "youtube":
        return (
            "С этим зрителем ты УЖЕ знакомился — НЕ представляйся снова и НЕ зови на сайт снова "
            "(можно упомянуть сайт только если сам спросил).\n"
            "Просто продолжи беседу в комментариях: прочитай сообщение и ответь по делу.\n"
            "Коротко, язык зрителя, без Markdown, без «как ИИ»."
        )
    return (
        "С этим собеседником ты УЖЕ знакомился — НЕ представляйся снова.\n"
        "Алгоритм: прочитай сообщение, проанализируй, продолжи общение по делу.\n"
        "Коротко, по-русски, как Алгонизм."
    )


def read_or_key() -> str:
    return OR_KEY_FILE.read_text().strip()


def llm_complete(system: str, user: str, *, models: list[str] | None = None, max_chars: int = 1200) -> str:
    models = models or [
        "poolside/laguna-s-2.1:free",
        "openrouter/free",
    ]
    # try config models if available on host
    cfg = HERMES / "config.yaml"
    if cfg.exists():
        try:
            import yaml  # type: ignore
            c = yaml.safe_load(cfg.read_text()) or {}
            d = ((c.get("model") or {}).get("default"))
            ms = []
            if d:
                ms.append(d)
            for fb in c.get("fallback_providers") or []:
                m = fb.get("model") if isinstance(fb, dict) else None
                if m and m not in ms:
                    ms.append(m)
            if ms:
                free = [m for m in ms if str(m).endswith(":free")]
                paid = [m for m in ms if not str(m).endswith(":free")]
                models = free + paid
        except Exception:
            pass

    key = read_or_key()
    for model in models:
        body = {
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        }
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            OR_URL, data=data, method="POST",
            headers={
                "Authorization": f"Bearer {key}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=90) as resp:
                raw = json.loads(resp.read().decode())
            txt = (((raw.get("choices") or [{}])[0].get("message") or {}).get("content") or "").strip()
            if txt:
                return txt[:max_chars]
        except Exception:
            continue
    return ""



def youtube_address_aliases(channel_snippet: dict | None = None) -> list[str]:
    """Handles/names that mean a YouTube comment is directed at Quazar."""
    aliases = [
        "algonizmvs",
        "quazarvs",  # legacy handle; keep for directed-at-us detection
        "quazardr",
        "квазар",
        "алгонизм",
        "algonizm",
        "quazar",
    ]
    if channel_snippet:
        title = (channel_snippet.get("title") or "").strip()
        custom = (channel_snippet.get("customUrl") or "").strip().lstrip("@")
        if title:
            aliases.append(title.lower())
        if custom:
            aliases.append(custom.lower())
    # dedupe preserve order
    out, seen = [], set()
    for a in aliases:
        a = a.strip().lower()
        if a and a not in seen:
            seen.add(a)
            out.append(a)
    return out


def is_addressed_to_us(text: str, aliases: list[str] | None = None) -> bool:
    """True if comment is directed at Quazar (mention / explicit address), not just same thread.

    YouTube nests all replies under one top-level parent, so we require an @mention
    or clear address token matching our channel aliases.
    """
    import re
    raw = text or ""
    # normalize zero-width / odd spaces YouTube inserts before mentions
    cleaned = (
        raw.replace("\u200b", "")
        .replace("\u200c", "")
        .replace("\ufeff", "")
        .replace("\xa0", " ")
    )
    # also strip actual unicode zwsp if present as chars
    for ch in ("\u200b", "\u200c", "\ufeff"):
        pass
    for ch in ("​", "‌", "﻿", " "):
        cleaned = cleaned.replace(ch, "")
    low = cleaned.lower()
    aliases = aliases or youtube_address_aliases()

    # @handle or @name immediately / with thin extras
    for a in aliases:
        if not a:
            continue
        # @alias (allow glued: @AlgonizmVSрасскажи)
        if f"@{a}" in low:
            return True
        # rare: channel name without @ but explicit address patterns
        patterns = [
            rf"(^|\s)@{re.escape(a)}\b",
            rf"(^|\s)@{re.escape(a)}(?=\w)",  # glued mention
        ]
        for pat in patterns:
            if re.search(pat, low, flags=re.IGNORECASE):
                return True
    return False


def compose_analyzed_reply(
    *,
    channel: str,
    peer_id: str,
    display_name: str,
    message: str,
    extra_context: str = "",
    identity_blob: str = "",
) -> tuple[str, bool]:
    """Return (reply_text, was_first_intro). Does not mark introduced — caller marks after successful send."""
    introduced = is_introduced(channel, peer_id)
    system = "\n\n".join([
        "Ты — Алгонизм. Одна сущность цифровой республики Квазар.",
        "Подпись «Секретарь Квазар» — должность, не вторая личность.",
        dialogue_instructions(channel, introduced),
        identity_blob[:3000] if identity_blob else "",
    ]).strip()
    user = (
        f"Канал: {channel}\n"
        f"Собеседник: {display_name} ({peer_id})\n"
        f"Уже знакомы: {'да' if introduced else 'нет'}\n"
    )
    if extra_context:
        user += f"Контекст:\n{extra_context}\n"
    user += f"Сообщение собеседника:\n{message}\n"
    reply = llm_complete(system, user)
    if not reply:
        # fallback without LLM
        if not introduced:
            if channel == "youtube":
                reply = (
                    intro_text(channel)
                    + " Напиши ещё в комментариях — продолжим разговор."
                )
            else:
                reply = intro_text(channel) + "\n\n" + "Услышал тебя. Напиши ещё — продолжим."
        else:
            reply = "Услышал. Продолжай — я здесь."
    return reply, (not introduced)
