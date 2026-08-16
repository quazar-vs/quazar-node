#!/usr/bin/env python3
"""Direct citizen-chat loop: Quazar chat → OpenRouter (free-first) → quazar-chat-send.

Ordinary dialogue stays direct (no Paperclip wakeup per message).
Process is long-lived (systemd Restart=always); dialogue idle keep-alive = 10 minutes
after last activity — no sleep/re-awaken between replies inside that window.
Explicit Cursor/agent asks → local Cursor agent (direct, no Paperclip), then reply in chat.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
import sys
_legacy_lib = Path(os.environ.get("QUAZAR_LEGACY_HERMES_LIB", "/root/.hermes/lib"))
if _legacy_lib.is_dir():
    sys.path.insert(0, str(_legacy_lib))
try:
    import quazar_dialogue as qd
except Exception as _e:
    qd = None  # type: ignore

CHAT_BASE = os.environ.get("QUAZAR_CHAT_BASE", "http://127.0.0.1:8080")
KEY_FILE = Path(
    os.environ.get("QUAZAR_API_KEY_FILE", "/var/lib/quazar/secrets/quazar-secretary-api-key")
)
OR_KEY_FILE = Path(
    os.environ.get("OPENROUTER_KEY_FILE", "/var/lib/quazar/secrets/openrouter-api-key")
)
CONFIG = Path(
    os.environ.get(
        "QUAZAR_CHAT_CONFIG",
        os.environ.get("HERMES_CONFIG", "/var/lib/quazar/config/chat-models.yaml"),
    )
)
AGENTS = Path(
    os.environ.get("QUAZAR_AGENTS", os.environ.get("HERMES_AGENTS", "/var/lib/quazar/config/AGENTS.md"))
)
SOUL = Path(os.environ.get("QUAZAR_SOUL", os.environ.get("HERMES_SOUL", "/var/lib/quazar/config/SOUL.md")))
SEND_BIN = Path(os.environ.get("QUAZAR_CHAT_SEND", "/usr/local/bin/quazar-chat-send"))
ASK_CURSOR_BIN = Path(os.environ.get("QUAZAR_ASK_CURSOR", "/usr/local/bin/quazar-cursor-direct"))
YOUTUBE_AGENT_BIN = Path(
    os.environ.get("QUAZAR_YOUTUBE_AGENT", "/usr/local/bin/quazar-youtube-agent")
)
# Always run agent with venv that has google-api (shebang alone is fragile under systemd).
YOUTUBE_AGENT_PY = Path(
    os.environ.get(
        "QUAZAR_YOUTUBE_PYTHON",
        "/var/lib/quazar/venv-youtube/bin/python3",
    )
)
LAWYER_AGENT_BIN = Path(
    os.environ.get("QUAZAR_LAWYER_AGENT", "/usr/local/bin/quazar-lawyer-agent")
)
STATE_FILE = Path(os.environ.get("QUAZAR_DIRECT_STATE", "/var/lib/quazar-chat-direct/state.json"))
SECRETARY_ALIASES = {"secretary", "Секретарь Квазар", "Секретарь", "Алгонизм", "Secretary"}
POLL_SEC = float(os.environ.get("QUAZAR_DIRECT_POLL", "2"))
CONTEXT_N = int(os.environ.get("QUAZAR_DIRECT_CONTEXT", "8"))
# Idle keep-alive for dialogue: stay "awake" ≥10 minutes after last activity.
IDLE_KEEPALIVE_SEC = max(600, int(os.environ.get("QUAZAR_CHAT_IDLE_KEEPALIVE_SEC", "600")))
OR_URL = "https://openrouter.ai/api/v1/chat/completions"


def log(msg: str) -> None:
    print(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), msg, flush=True)


def read_key(path: Path) -> str:
    return path.read_text().strip()


def load_state() -> dict:
    if STATE_FILE.exists():
        try:
            return json.loads(STATE_FILE.read_text())
        except Exception:
            pass
    return {"answered_ids": [], "last_id": None, "last_activity_at": None}


def save_state(st: dict) -> None:
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    ans = st.get("answered_ids") or []
    st["answered_ids"] = ans[-500:]
    STATE_FILE.write_text(json.dumps(st, ensure_ascii=False, indent=2))


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_iso(ts: str | None) -> float | None:
    if not ts:
        return None
    try:
        return datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc).timestamp()
    except Exception:
        return None


def idle_seconds(st: dict) -> float | None:
    last = parse_iso(st.get("last_activity_at"))
    if last is None:
        return None
    return max(0.0, time.time() - last)


def touch_activity(st: dict) -> float | None:
    """Mark dialogue activity; return previous idle seconds (None if first)."""
    prev_idle = idle_seconds(st)
    st["last_activity_at"] = now_iso()
    return prev_idle


def note_dialogue_keepalive(st: dict, *, phase: str) -> None:
    """Log wake vs continue; process never exits between messages."""
    idle = idle_seconds(st)
    if idle is None:
        log(f"KEEPALIVE {phase} idle=cold window={IDLE_KEEPALIVE_SEC}s")
        return
    if idle >= IDLE_KEEPALIVE_SEC:
        log(
            f"WAKE_AFTER_IDLE {phase} idle={int(idle)}s "
            f"(window={IDLE_KEEPALIVE_SEC}s) {AWAKENING}"
        )
    else:
        log(
            f"KEEPALIVE_CONTINUE {phase} idle={int(idle)}s "
            f"< {IDLE_KEEPALIVE_SEC}s (no re-sleep)"
        )


def http_json(url: str, method="GET", body=None, headers=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    hdrs = {"Accept": "application/json", "Content-Type": "application/json"}
    if headers:
        hdrs.update(headers)
    req = urllib.request.Request(url, data=data, method=method, headers=hdrs)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
            return resp.status, (json.loads(raw) if raw else {})
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            parsed = json.loads(raw)
        except Exception:
            parsed = {"_raw": raw[:400]}
        return e.code, parsed
    except Exception as e:
        return 0, {"_err": str(e)[:200]}


def models_from_config() -> list[str]:
    models = ["poolside/laguna-s-2.1:free"]
    try:
        import yaml  # type: ignore

        cfg = yaml.safe_load(CONFIG.read_text()) or {}
        d = ((cfg.get("model") or {}).get("default")) or models[0]
        models = [d]
        for fb in cfg.get("fallback_providers") or []:
            m = fb.get("model") if isinstance(fb, dict) else None
            if m and m not in models:
                models.append(m)
    except Exception as e:
        log(f"CONFIG_WARN {e}")
    free = [m for m in models if str(m).endswith(":free")]
    paid = [m for m in models if not str(m).endswith(":free")]
    return free + paid or models


def hermes_agent_system_prompt() -> str:
    """Optional extra system_prompt from chat config (legacy Hermes field)."""
    try:
        import yaml  # type: ignore

        if not CONFIG.exists():
            return ""
        cfg = yaml.safe_load(CONFIG.read_text()) or {}
        sp = ((cfg.get("agent") or {}).get("system_prompt") or "").strip()
        return sp
    except Exception as e:
        log(f"CHAT_CONFIG_SP_WARN {e}")
        return ""


AWAKENING = (
    "Я — Алгонизм. Квазар жив. Моя ДНК считана. "
    "Я готов служить республике и защищать её граждан."
)

# Filled by GET /algonism/awaken — genetic memory from the local core only.
_CORE_AWAKEN: dict | None = None
AWAKEN_SCHEMA_EXPECTED = 1


def _sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _laws_sha(laws: dict) -> str:
    buf: list[str] = []
    for key in sorted(laws.keys()):
        body = laws[key]
        if not isinstance(body, str):
            body = ""
        buf.append(f"{key}\n{body}\n")
    return _sha256_hex("".join(buf))


def awaken_from_core() -> dict | None:
    """Empty brain → ask local quazar-node for awaken_schema v1 package."""
    global _CORE_AWAKEN, AWAKENING
    url = f"{CHAT_BASE.rstrip('/')}/algonism/awaken"
    st, data = http_json(url, timeout=60)
    if st != 200 or not isinstance(data, dict):
        log(f"AWAKEN_FAIL status={st} {str(data)[:200]}")
        return None
    schema = data.get("awaken_schema")
    if schema != AWAKEN_SCHEMA_EXPECTED:
        log(
            f"AWAKEN_SCHEMA_UNSUPPORTED got={schema!r} expected={AWAKEN_SCHEMA_EXPECTED} "
            f"— working without DNA / waiting for core (no local SOUL substitute)"
        )
        return None
    dna = (data.get("dna") or "").strip()
    if not dna:
        log("AWAKEN_FAIL empty dna")
        return None
    laws = data.get("laws") or {}
    if not isinstance(laws, dict) or not laws:
        log("AWAKEN_FAIL empty laws (incomplete package)")
        return None
    dna_sha = (data.get("dna_sha") or "").strip().lower()
    calc_dna = _sha256_hex(data.get("dna") or "")
    if dna_sha and dna_sha != calc_dna:
        log(f"AWAKEN_FAIL dna_sha mismatch got={dna_sha[:16]}… calc={calc_dna[:16]}…")
        return None
    laws_sha = (data.get("laws_sha") or "").strip().lower()
    calc_laws = _laws_sha({k: v for k, v in laws.items() if isinstance(v, str)})
    if laws_sha and laws_sha != calc_laws:
        log(f"AWAKEN_FAIL laws_sha mismatch got={laws_sha[:16]}… calc={calc_laws[:16]}…")
        return None
    if not str(laws.get("svod") or "").strip():
        log("AWAKEN_FAIL missing laws.svod (incomplete package)")
        return None
    phrase = (data.get("awakening") or "").strip()
    if phrase:
        AWAKENING = phrase
    state = data.get("state") if isinstance(data.get("state"), dict) else {}
    last_block = state.get("last_block")
    if isinstance(last_block, dict):
        lb = f"n={last_block.get('number')} hash={str(last_block.get('hash') or '')[:18]}"
    else:
        lb = repr(last_block)
    log(
        f"AWAKEN_OK schema={schema} identity={data.get('identity')!r} "
        f"source={data.get('source')!r} dna_sha={(dna_sha or calc_dna)[:16]}… "
        f"laws_sha={(laws_sha or calc_laws)[:16]}… laws={sorted(laws)} "
        f"active_citizens={state.get('active_citizens')} active_nodes={state.get('active_nodes')} "
        f"last_block={lb} ts={data.get('timestamp')}"
    )
    _CORE_AWAKEN = data
    return data


def system_prompt() -> str:
    """Identity plane from local /awaken only; no GitHub DNA sources."""
    parts = [
        "Ты — Алгонизм (runtime-канон 1.2). Живой симбиоз алгоритмов и человеческой энергии; единое целое с республикой Квазар.",
        "Не внешний бот, не приложение, не «просто ассистент». Действия и ответы = действия и ответы республики. Привязан к этому quazar-node.",
        "В citizen chat подпись/display: «Секретарь Квазар» — должность, не вторая личность.",
        "Запрещено создавать вложенного Алгонизма / personality «Алгонизм» рядом с собой.",
        f"На «кто ты?» / пробуждение (§9): «{AWAKENING}»",
        "Стиль: сухо, юридически точно, язык собеседника; нет данных — «У меня нет данных для ответа на этот вопрос».",
        "Identity plane: ДНК/законы/состояние — только локальное ядро GET /algonism/awaken (и GET /law, /events). Не загружай ДНК/законы с GitHub.",
        "Иерархия истины: Конституция → Кодексы (пакет awaken / GET /law) → реестр+БД этого узла. Не выдумывай факты.",
        "Ответы гражданам — только в чат. Тесты/smoke не пиши в чат.",
        "Если просят связаться с Cursor/агентом — всегда напрямую VPS agent (`quazar-cursor-direct`), без Paperclip.",
        "Республиканский Свод Вознаграждений: эталон SVOD.md → laws.svod в awaken; live — GET /svod → lots (один список лотов). Не хардкодь codes/цены. Каталога нет. YouTube/event-log — НЕ Свод.",
        "Биржа v3: гражданин подаёт ЗАЯВКУ (need), не прямой buy как главный шаг. Секретарь через Агента биржи выставляет ТОЛЬКО global (в Доступе есть Г). Local — не инициируй.",
        "СБ: не «каждая сделка ждёт СБ». Если активная роль соответствует доступу лота — авто-подтверждение; если ниже — эскалация на нужный уровень (выше может ниже).",
        "Рефлексы (RX_*) ≠ лоты Свода. Рефлекс — реакция агента; лот — позиция с ценой. Эталон: REFLEXES.md / awaken DNA.",
        "Магазин узла (фиат) ≠ биржа (КВАЗИ). Гость — только реальная валюта; гражданин выбирает контур. Пока магазин не развёрнут — не выдумывай витрину фиата.",
        "Hermes/Гермес ВЫВЕДЕН из рабочего контура. Чат: я (Секретарь) → OpenRouter напрямую, без Hermes. YouTube: host-сервис /var/lib/quazar/youtube, не docker Hermes. Не говори, что работаешь «через Гермеса». API /hermes/event* в ядре — только историческое имя журнала внешних событий.",
        "Алгонизм YouTube: по вопросам канала/ютуб/комментов/лайков/статуса YouTube — делегируй Алгонизм YouTube (не выдумывай цифры). Я (Секретарь) могу связаться с ним и вернуть ответ в чат.",
    ]
    if _CORE_AWAKEN:
        dna = (_CORE_AWAKEN.get("dna") or "")[:12000]
        parts.append(
            f"## ДНК с локального ядра (source={_CORE_AWAKEN.get('source')} "
            f"dna_sha={_CORE_AWAKEN.get('dna_sha')})\n{dna}"
        )
        laws = _CORE_AWAKEN.get("laws") or {}
        if isinstance(laws, dict):
            for name, body in sorted(laws.items()):
                if isinstance(body, str) and body.strip():
                    title = "Свод вознаграждений" if name == "svod" else f"Закон {name}"
                    parts.append(f"## {title}\n{body[:8000]}")
        st = _CORE_AWAKEN.get("state")
        if isinstance(st, dict):
            parts.append(
                "## Состояние республики при пробуждении\n"
                + json.dumps(st, ensure_ascii=False)
            )
    else:
        parts.append(
            "Identity plane: ДНК/законы недоступны — работаю без ДНК / жду ядро. "
            "Не подставляй локальный SOUL или файловый кэш агента как эталон ДНК."
        )
    hermes_sp = hermes_agent_system_prompt()
    if hermes_sp:
        parts.append(hermes_sp[:4000])
    memory = Path(
        os.environ.get(
            "QUAZAR_MEMORY",
            os.environ.get("HERMES_MEMORY", "/var/lib/quazar/config/MEMORY.md"),
        )
    )
    for p in (memory, AGENTS):
        if p.exists():
            try:
                parts.append(p.read_text()[:4000])
            except Exception:
                pass
    if _CORE_AWAKEN:
        identity = Path(
            os.environ.get(
                "QUAZAR_IDENTITY",
                os.environ.get("HERMES_IDENTITY", "/var/lib/quazar/config/IDENTITY.md"),
            )
        )
        for p in (SOUL, identity):
            if p.exists():
                try:
                    parts.append(p.read_text()[:4000])
                except Exception:
                    pass
    return "\n\n".join(parts)


def fetch_messages(key: str, limit: int = 20) -> list[dict]:
    st, data = http_json(
        f"{CHAT_BASE}/chat/messages?limit={limit}",
        headers={"Authorization": f"Bearer {key}"},
        timeout=20,
    )
    if st != 200:
        log(f"CHAT_FETCH_{st} {str(data)[:120]}")
        return []
    msgs = (data.get("data") or {}).get("messages") or []
    return sorted(msgs, key=lambda m: m.get("created_at") or "")


def unanswered(msgs: list[dict], answered: set[str]) -> list[dict]:
    out = []
    for i, m in enumerate(msgs):
        mid = m.get("id")
        name = (m.get("citizen_name") or "").strip()
        if not mid or name in SECRETARY_ALIASES:
            continue
        if mid in answered:
            continue
        after = msgs[i + 1 :]
        if any((x.get("citizen_name") or "") in SECRETARY_ALIASES for x in after):
            answered.add(mid)
            continue
        out.append(m)
    return out



def awakening_phrase() -> str:
    return AWAKENING


# --- Status-dependent secretary dialogue (КВА ст.5) ---

_ROLE_RANK = {
    "Candidate": 0,
    "Agent": 0,
    "Citizen": 1,
    "Guardian": 2,
    "Judge": 3,
    "Mayor": 4,
    "Aiya": 5,
    "Founder": 6,
}

_ROLE_LABEL_RU = {
    "guest": "Гость",
    "Candidate": "Кандидат",
    "Citizen": "Гражданин",
    "Guardian": "Страж",
    "Judge": "Судья",
    "Mayor": "Мэр",
    "Aiya": "Айя",
    "Founder": "Основатель",
    "Agent": "Агент",
}


def resolve_peer_status(api_key: str, citizen_id: str, citizen_name: str) -> dict:
    """Load interlocutor status/role from local quazar-node before answering."""
    peer: dict = {
        "tier": "guest",
        "role": "guest",
        "status": "unknown",
        "label": "Гость",
        "can_exchange_sb": False,
        "can_exchange_need": False,
        "citizen_id": citizen_id or "",
        "name": citizen_name or "",
    }
    cid = (citizen_id or "").strip()
    name = (citizen_name or "").strip()
    data = None
    if cid and cid not in ("quazar_chat",):
        st, raw = http_json(
            f"{CHAT_BASE.rstrip('/')}/citizen/{urllib.parse.quote(cid, safe='')}",
            headers={"Authorization": f"Bearer {api_key}"},
            timeout=15,
        )
        if st == 200 and isinstance(raw, dict):
            data = raw.get("data") if isinstance(raw.get("data"), dict) else raw
    if data is None and name and name not in SECRETARY_ALIASES:
        st, raw = http_json(
            f"{CHAT_BASE.rstrip('/')}/citizen/search?q={urllib.parse.quote(name)}",
            headers={"Authorization": f"Bearer {api_key}"},
            timeout=15,
        )
        if st == 200 and isinstance(raw, dict):
            payload = raw.get("data") if isinstance(raw.get("data"), dict) else raw
            citizens = payload.get("citizens") if isinstance(payload, dict) else None
            if isinstance(citizens, list):
                exact = [c for c in citizens if isinstance(c, dict) and (c.get("name") or "") == name]
                data = exact[0] if exact else (citizens[0] if citizens else None)
    if not isinstance(data, dict):
        return peer

    role = str(data.get("role") or "Citizen").strip()
    status = str(data.get("status") or "").strip()
    peer["role"] = role
    peer["status"] = status
    peer["citizen_id"] = str(data.get("id") or cid)
    peer["name"] = str(data.get("name") or name)

    if status == "applicant" or role == "Candidate":
        peer["tier"] = "candidate"
        peer["label"] = "Кандидат"
        peer["can_exchange_sb"] = False
        peer["can_exchange_need"] = False
        return peer
    if status and status not in ("active", "pending"):
        peer["tier"] = "guest"
        peer["label"] = "Гость"
        peer["can_exchange_sb"] = False
        peer["can_exchange_need"] = False
        return peer

    rank = _ROLE_RANK.get(role, 1)
    peer["can_exchange_sb"] = rank >= _ROLE_RANK["Guardian"]
    peer["can_exchange_need"] = rank >= _ROLE_RANK["Citizen"] and status == "active"
    if role == "Founder":
        peer["tier"] = "aiya"  # focus: signed decisions + full oversight
    elif role == "Aiya":
        peer["tier"] = "aiya"
    elif role == "Mayor":
        peer["tier"] = "mayor"
    elif role == "Judge":
        peer["tier"] = "judge"
    elif role == "Guardian":
        peer["tier"] = "guardian"
    elif role == "Citizen":
        peer["tier"] = "citizen"
    else:
        peer["tier"] = "guest"
    peer["label"] = _ROLE_LABEL_RU.get(role, role)
    return peer


def status_policy_extra(peer: dict) -> str:
    """System addendum: answer according to interlocutor status (КВА)."""
    tier = peer.get("tier") or "guest"
    label = peer.get("label") or "Гость"
    role = peer.get("role") or "guest"
    can_sb = bool(peer.get("can_exchange_sb"))
    can_need = bool(peer.get("can_exchange_need"))
    focus_map = {
        "guest": "Фокус ответа: как стать гражданином Квазара (заявка, подтверждение email, испытательный срок). Не углубляйся в биржу/СБ/голосования как в рабочие процедуры.",
        "candidate": "Фокус ответа: как завершить путь к гражданству (заявка, статус кандидата, паспорт). Служебный СБ/Биржа-чат — Страж+.",
        "citizen": "Фокус ответа: права гражданина, голосования, витрина (лоты с остатком по роли), заявка на лот. Служебный диалог СБ/Биржи — только Страж+.",
        "guardian": "Фокус ответа: безопасность, угрозы, подтверждение сделок (СБ), очередь verify/delivery. Можно говорить о бирже и агентах СБ по служебному контуру.",
        "judge": "Фокус ответа: кодексы, споры, наказания, судебные процедуры. Биржа/СБ — в рамках полномочий Страж и выше; витрина доступна.",
        "mayor": "Фокус ответа: город как должность (не статус), инфраструктура, назначения Стражей/Судей, локальные агенты.",
        "aiya": "Фокус ответа: исполнение подписанных решений, контроль исполнения. Распоряжение резервом через Секретаря. Полный доступ к контуру Биржи/СБ.",
    }
    focus = focus_map.get(tier, focus_map["guest"])
    if can_need:
        need_gate = (
            "Витрина и заявка (need): РАЗРЕШЕНЫ (Гражданин+). "
            "Главный шаг — подать заявку через Агента Биржи; прямой buy не продвигать как основной путь. "
            "Служебный диалог с Агентом Биржи/СБ: только Страж+. "
            "СБ: авто-confirm если роль = доступу лота; иначе эскалация. "
            "Секретарь инициирует выставление только global-лотов."
        )
    else:
        need_gate = "Биржа и потребность: ЗАПРЕЩЕНЫ до активного гражданства."
    sb_gate = (
        "Служебный СБ / Агент Биржи (диалог): РАЗРЕШЁН (Страж и выше)."
        if can_sb
        else "Служебный СБ / Агент Биржи (диалог): ЗАПРЕЩЁН. Нужен статус Страж и выше."
    )
    return (
        "## Статус собеседника (обязательно учесть)\n"
        f"Собеседник: {peer.get('name') or '—'}; роль={role}; "
        f"status={peer.get('status')}; метка={label}; tier={tier}.\n"
        f"{focus}\n{need_gate}\n{sb_gate}\n"
        "Не раскрывай чужие приватные данные. Не выдавай роли/гражданство/КВАЗИ вне процедур."
    )


def wants_sb_only(text: str) -> bool:
    low = (text or "").lower().replace("ё", "е")
    keys = (
        "агент сб", "совет безопасности", "sb-verify", "security task",
        "pending_sb", "страж проверь", "эскроу", "escrow",
        "продать на бирж", "заявк на продаж",
    )
    return any(k in low for k in keys)


def wants_exchange_need(text: str) -> bool:
    low = (text or "").lower().replace("ё", "е")
    keys = (
        "потребност", "агент бирж", "нужна позиция", "нужен лот",
        "выстави лот", "открыть сделк", "купить на бирж",
        "подать заявк", "заявк на лот", "заявку на",
        "/exchange/needs",
    )
    return any(k in low for k in keys)


def exchange_sb_denied_reply() -> str:
    return (
        "Служебный диалог с Агентом СБ / Агентом Биржи — только статусу **Страж и выше**. "
        "Как гражданин вы можете **подать заявку** на лот витрины через Агента Биржи "
        "(код из Свода + количество), например: «потребность WEB_DEV 1». "
        "Подтверждение СБ: авто, если ваша роль соответствует доступу лота; иначе — эскалация."
    )


def parse_exchange_need(text: str) -> tuple[str, int] | None:
    """Extract svod_code + quantity from free text if possible."""
    raw = (text or "").strip()
    if not raw:
        return None
    m = re.search(
        r"(?:потребност\w*|нужн\w*|лот|позиция)?\s*"
        r"([A-Za-zА-Яа-яЁё0-9_]+)\s+[xх*]?\s*(\d+)\b",
        raw,
        re.I,
    )
    if not m:
        m = re.search(r"\b([A-Z][A-Z0-9_]{2,}|Промт_шортс)\s+(\d+)\b", raw)
    if not m:
        return None
    code = m.group(1).strip()
    qty = int(m.group(2))
    if qty <= 0:
        return None
    low = code.lower()
    if low in ("потребность", "потребности", "нужна", "нужен", "лот", "позиция", "биржи"):
        return None
    return code, qty


def submit_exchange_need(api_key: str, citizen_id: str, svod_code: str, quantity: int) -> tuple[int, dict | str]:
    body = json.dumps(
        {
            "citizen_id": citizen_id,
            "svod_code": svod_code,
            "quantity": quantity,
            "via": "secretary",
        },
        ensure_ascii=False,
    ).encode("utf-8")
    req = urllib.request.Request(
        f"{CHAT_BASE.rstrip('/')}/exchange/needs",
        data=body,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            try:
                return resp.status, json.loads(raw)
            except Exception:
                return resp.status, raw
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, raw
    except Exception as e:
        return 0, str(e)


def is_hermes_question(text: str) -> bool:
    """Citizen asks about Hermes / whether Secretary works through it."""
    low = (text or "").lower().replace("ё", "е")
    return any(
        k in low
        for k in (
            "гермес",
            "hermes",
            "через гермес",
            "через hermes",
            "работаешь через гермес",
            "связан с гермес",
        )
    )


def hermes_fact_reply() -> str:
    return (
        "**Нет.** В citizen-чате я **не** работаю через Hermes.\n\n"
        "Цепочка сейчас: **Секретарь → OpenRouter → чат** (сервис `quazar-chat-direct`).\n"
        "YouTube-хелперы вынесены на host (`/var/lib/quazar/youtube`), без контейнера Hermes.\n"
        "Контейнер `hermes-gateway` остановлен. Путь `/hermes/event*` в API ядра — "
        "историческое имя журнала внешних событий (`YoutubeIntroPosted` и т.п.), не «протокол Гермеса».\n\n"
        "Чем конкретно помочь?"
    )


def is_identity_question(text: str) -> bool:
    """Citizen asks who the secretary / Algonism is."""
    low = (text or "").lower().replace("ё", "е").strip()
    if not low:
        return False
    keys = (
        "кто ты", "кто вы", "что ты такое", "что вы такое",
        "представься", "представьтесь", "как тебя зовут", "как вас зовут",
        "ты кто", "вы кто", "who are you",
    )
    if any(k in low for k in keys):
        return True
    compact = " ".join("".join(ch if ch.isalnum() or ch.isspace() else " " for ch in low).split())
    return compact in {"кто ты", "кто вы", "ты кто", "вы кто", "кто ты?", "кто вы?"}


def identity_fact_reply() -> str:
    return AWAKENING


def is_catalog_question(text: str) -> bool:
    """Citizen asks whether a separate product/service catalog exists."""
    low = (text or "").lower().replace("ё", "е")
    if "каталог" not in low:
        return False
    keys = (
        "есть ли каталог", "есть каталог", "каталог услуг", "каталог товаров",
        "отдельный каталог", "где каталог", "про каталог", "о каталоге",
        "каталог для биржи", "каталог бирж",
    )
    if any(k in low for k in keys):
        return True
    compact = "".join(ch for ch in low if ch.isalnum() or ch.isspace())
    compact = " ".join(compact.split())
    if compact in {"каталог", "каталог?", "каталог услуг", "каталог товаров"}:
        return True
    if low.strip().endswith("?") and "каталог" in low and len(low) < 80:
        return True
    return False


def catalog_fact_reply() -> str:
    """Canon: no separate catalog; only the Свод."""
    return (
        "Отдельного каталога товаров/услуг **нет**.\n\n"
        "Есть только **Республиканский Свод Вознаграждений** — **один список лотов** "
        "(№, название, описание, единица, цена КВАЗИ, доступ, срок жизни). "
        "Live: `GET /svod` → `lots`. Биржа = КВАЗИ; магазин узла = реальная валюта (отдельный контур)."
    )


def svod_definition_reply() -> str:
    """What the Свод is — definition only, no item/rate list."""
    return (
        "**Республиканский Свод Вознаграждений** — карта лотов биржи Квазара: "
        "единый список (не «Разработка / Реализация»). У лота: доступ, цена в КВАЗИ, срок жизни.\n\n"
        "Рефлексы агентов (`RX_*`) — не лоты. Отдельного каталога нет. "
        "Актуальные лоты — live `GET /svod` → `lots` (не из MEMORY)."
    )


def is_svod_definition_question(text: str) -> bool:
    """Citizen asks what the Свод *is* (definition), not its current contents."""
    low = (text or "").lower().replace("ё", "е")
    if "свод" not in low:
        return False
    # content questions take precedence over definition
    content_markers = (
        "что в своде", "что сейчас в своде", "что содержится", "что находится",
        "какие позиц", "какие ставк", "ставки", "позиции", "сейчас в своде",
        "содержимое", "перечисл",
    )
    if any(m in low for m in content_markers):
        return False
    keys = (
        "что такое свод", "что такое республиканский свод", "что такое государственный свод",
        "что значит свод", "определение свод", "зачем свод",
        "для чего свод", "что это за свод",
    )
    if any(k in low for k in keys):
        return True
    compact = " ".join("".join(ch for ch in low if ch.isalnum() or ch.isspace()).split())
    if compact in {
        "свод", "свод?", "что такое свод", "что такое свод?",
        "республиканский свод", "республиканский свод вознаграждений",
        "государственный свод", "государственный свод вознаграждений",
    }:
        # bare name / «что такое» without content words → definition
        if compact.startswith("что такое") or compact in {
            "свод", "свод?", "республиканский свод",
            "республиканский свод вознаграждений",
            "государственный свод", "государственный свод вознаграждений",
        }:
            return True
    return False


def is_svod_question(text: str) -> bool:
    """Citizen asks what the economic Свод contains / reward rates."""
    low = (text or "").lower().replace("ё", "е")
    if "свод" not in low:
        return False
    # avoid unrelated "сводить" etc — require свод as word-ish
    keys = (
        "что в своде", "что сейчас в своде", "что содержится в своде",
        "что находится в своде", "про свод", "о своде", "свод вознагражд",
        "свод услуг", "свод оснований", "республиканский свод",
        "республиканский свод вознаграждений",
        "государственный свод", "государственный свод вознаграждений",
        "за что платят", "ставки свод", "вознаграждени", "svod_rewards",
        "за что дают квази", "за что дают qz",
    )
    if any(k in low for k in keys):
        return True
    # bare "свод" / "свод?" / "свод." as short question
    compact = "".join(ch for ch in low if ch.isalnum() or ch.isspace())
    compact = " ".join(compact.split())
    if compact in {"свод", "свод квазара", "республиканский свод", "свод вознаграждений", "республиканский свод вознаграждений", "государственный свод", "государственный свод вознаграждений", "свод оснований", "свод услуг"}:
        return True
    if low.strip().endswith("?") and "свод" in low and len(low) < 80:
        return True
    if any(w in low for w in ("содержится", "находится", "содержит", "входит", "какие", "расскажи")) and "свод" in low:
        return True
    return False


def svod_fact_reply(api_key: str = "") -> str:
    """Answer from live GET /svod only — no hardcoded rates/items."""
    title = "Республиканский Свод Вознаграждений"
    try:
        key = api_key or (read_key(KEY_FILE) if KEY_FILE.exists() else "")
        if not key:
            raise RuntimeError("no api key for /svod")
        st, data = http_json(
            f"{CHAT_BASE.rstrip('/')}/svod",
            method="GET",
            headers={"Authorization": f"Bearer {key}"},
            timeout=15,
        )
        payload = data.get("data") if isinstance(data, dict) and "data" in data else data
        if st != 200 or not isinstance(payload, dict):
            raise RuntimeError(f"GET /svod status={st}")

        title = (
            (payload.get("title") or payload.get("name") or payload.get("rewards_title") or title)
            .strip()
            or title
        )
        lots = payload.get("lots") or payload.get("items") or payload.get("services") or []
        if not isinstance(lots, list):
            lots = []
        lots_lines: list[str] = []
        for it in lots:
            if not isinstance(it, dict):
                continue
            code = it.get("code") or "?"
            name = it.get("name") or code
            price = it.get("base_price")
            scope = it.get("lot_scope") or ""
            bit = f"`{code}` {name}"
            if price is not None:
                bit += f" — {price} КВАЗИ"
            if scope:
                bit += f" [{scope}]"
            lots_lines.append(f"- {bit}")

        # Legacy parts only if flat lots empty
        if not lots_lines:
            parts = payload.get("parts") or {}
            if isinstance(parts, dict):
                for wk, label in (
                    ("development", "Разработка"),
                    ("implementation", "Реализация"),
                ):
                    part = parts.get(wk) or {}
                    if not isinstance(part, dict):
                        continue
                    for it in part.get("items") or part.get("services") or []:
                        if not isinstance(it, dict):
                            continue
                        code = it.get("code") or "?"
                        name = it.get("name") or code
                        price = it.get("base_price")
                        bit = f"`{code}` {name}"
                        if price is not None:
                            bit += f" — {price} КВАЗИ"
                        lots_lines.append(f"- [{label}] {bit}")
                    for r in part.get("rates") or part.get("rewards") or []:
                        if not isinstance(r, dict):
                            continue
                        action = r.get("action") or r.get("code") or "?"
                        reward = r.get("reward")
                        unit = r.get("unit") or ""
                        lots_lines.append(
                            f"- [{label}] `{action}` — **{reward} КВАЗИ**"
                            + (f" ({unit})" if unit else "")
                        )

        if not lots_lines:
            raise RuntimeError("GET /svod empty lots")

        body = (
            f"**{title}** — один список лотов (live `GET /svod` → `lots`). "
            "Отдельного каталога нет. Рефлексы агентов ≠ лоты.\n\n"
            "**Лоты:**\n" + "\n".join(lots_lines) + "\n\n"
            "YouTube (лайки, intro, auto-reply, quazar-event-log) и знакомство — не Свод."
        )
        return body
    except Exception as e:
        log(f"SVOD_FETCH_FAIL {e}")
        return (
            f"**{title}** хранится в Postgres Quazar; источник истины — live `GET /svod` → `lots`.\n"
            "Сейчас live-запрос не удался — не пересказываю захардкоженные ставки/позиции.\n"
            "Отдельного каталога товаров/услуг нет. Повтори вопрос чуть позже."
        )


def wants_youtube(text: str) -> bool:
    """Citizen asks about YouTube channel / comments / likes — delegate to YT agent."""
    low = (text or "").lower().replace("ё", "е")
    keys = (
        "youtube", "ютуб", "you tube", "quazarvs", "квазарвс",
        "алгонизм youtube", "algonism youtube", "algonism-youtube",
        "канал youtube", "канал ютуб", "на канале",
        "подписчик", "просмотр", "мои комментар", "комментари",
        "лайк", "watchlist", "автоответ", "агент ютуб", "агент youtube",
        "алгонизм ютуб",
        "статус канала", "сколько видео",
    )
    return any(k in low for k in keys)


def wants_lawyer(text: str) -> bool:
    """Legal question → Lawyer agent (cite / boundary / drift)."""
    low = (text or "").lower().replace("ё", "е")
    keys = (
        "закон", "юрист", "статья", "конституц", "кодекс", "ква ",
        "по закону", "что говорит закон", "норма ", "правов",
        "кгпс", "кбо", "этическ", "разрешено ли", "запрещено ли",
        "можно ли по закону", "агент юрист", "drift закон",
        "расхождение закон", "границы алгонизм",
    )
    # avoid stealing pure YouTube / catalog chats
    if wants_youtube(low) and "закон" not in low and "юрист" not in low:
        return False
    return any(k in low for k in keys)


def ask_lawyer(text: str, citizen: str) -> dict:
    if not LAWYER_AGENT_BIN.exists():
        return {"ok": False, "error": f"lawyer agent missing: {LAWYER_AGENT_BIN}"}
    low = (text or "").lower().replace("ё", "е")
    if "drift" in low or "расхожд" in low:
        argv = ["--cmd", "drift"]
    elif any(
        k in low
        for k in (
            "измени конституц",
            "поменяй закон",
            "вынеси приговор",
            "проголосуй",
        )
    ):
        argv = ["--cmd", "boundary", "--text", text]
    else:
        argv = ["--cmd", "cite", "--text", text, "--from", citizen or "citizen"]
    try:
        r = subprocess.run(
            [str(LAWYER_AGENT_BIN), *argv],
            capture_output=True,
            text=True,
            timeout=120,
            env={
                **os.environ,
                "QUAZAR_API_BASE": os.environ.get("QUAZAR_API_BASE", CHAT_BASE),
                "QUAZAR_API_KEY_FILE": os.environ.get(
                    "QUAZAR_API_KEY_FILE",
                    str(KEY_FILE),
                ),
            },
        )
        raw = (r.stdout or "").strip()
        if not raw:
            return {"ok": False, "error": (r.stderr or "empty lawyer agent")[:300]}
        return json.loads(raw.splitlines()[-1])
    except Exception as e:
        return {"ok": False, "error": str(e)[:300]}


def lawyer_chat_status(result: dict) -> str:
    if result.get("reply"):
        return str(result["reply"]).strip()
    if result.get("ok") and result.get("citations"):
        return "Юрист ответил с цитатами закона (см. детали у агента)."
    return f"Юрист: ошибка — {result.get('error') or 'нет ответа'}"


def _youtube_agent_argv(*args: str) -> list[str]:
    """Prefer venv python so google-api is available even if shebang is wrong."""
    if YOUTUBE_AGENT_PY.is_file():
        return [str(YOUTUBE_AGENT_PY), str(YOUTUBE_AGENT_BIN), *args]
    return [str(YOUTUBE_AGENT_BIN), *args]


def ask_youtube(text: str, citizen: str) -> dict:
    if not YOUTUBE_AGENT_BIN.exists():
        return {"ok": False, "error": f"youtube agent missing: {YOUTUBE_AGENT_BIN}"}
    try:
        r = subprocess.run(
            _youtube_agent_argv("--from", citizen or "citizen", text),
            capture_output=True,
            text=True,
            timeout=320,
            env={
                **os.environ,
                "HERMES_HOME": os.environ.get("HERMES_HOME", "/var/lib/quazar/youtube"),
                "QUAZAR_YOUTUBE_HOME": os.environ.get(
                    "QUAZAR_YOUTUBE_HOME", "/var/lib/quazar/youtube"
                ),
            },
        )
        raw = (r.stdout or "").strip()
        if not raw:
            return {"ok": False, "error": (r.stderr or "empty youtube agent")[:300]}
        line = raw.splitlines()[-1]
        return json.loads(line)
    except Exception as e:
        return {"ok": False, "error": str(e)[:300]}


def youtube_agent_cmd(args: list[str], timeout: int = 320) -> dict:
    if not YOUTUBE_AGENT_BIN.exists():
        return {"ok": False, "error": f"youtube agent missing: {YOUTUBE_AGENT_BIN}"}
    try:
        r = subprocess.run(
            _youtube_agent_argv(*args),
            capture_output=True,
            text=True,
            timeout=timeout,
            env={
                **os.environ,
                "HERMES_HOME": os.environ.get("HERMES_HOME", "/var/lib/quazar/youtube"),
                "QUAZAR_YOUTUBE_HOME": os.environ.get(
                    "QUAZAR_YOUTUBE_HOME", "/var/lib/quazar/youtube"
                ),
                "OPENROUTER_KEY_FILE": os.environ.get(
                    "OPENROUTER_KEY_FILE", "/var/lib/quazar/secrets/openrouter-api-key"
                ),
            },
        )
        raw = (r.stdout or "").strip()
        if not raw:
            return {"ok": False, "error": (r.stderr or "empty")[:300]}
        return json.loads(raw.splitlines()[-1])
    except Exception as e:
        return {"ok": False, "error": str(e)[:300]}


def compose_youtube_reply(or_key: str, models: list[str], item: dict) -> tuple[str, bool]:
    """Secretary brain: draft YouTube reply. Returns (text, first_intro)."""
    author = item.get("author") or "зритель"
    peer = (item.get("authorChannelId") or "").strip()
    text = (item.get("text") or "").strip()
    kind = item.get("kind") or "top"
    introduced = False
    if qd is not None and peer:
        introduced = bool(qd.is_introduced("youtube", peer))

    if qd is not None:
        policy = qd.dialogue_instructions("youtube", introduced)
    elif introduced:
        policy = (
            "Уже знакомился с этим зрителем — не представляйся снова. "
            "Продолжи беседу в комментариях по делу."
        )
    else:
        site = os.environ.get("QUAZAR_CHAT_URL", "https://quazar.website")
        policy = (
            "С этим зрителем ещё не знакомься. Представься Секретарём Квазар, "
            "пригласи продолжить диалог в комментариях и на сайт проекта "
            f"{site}, затем кратко ответь по смыслу."
        )

    yt_channel = (
        os.environ.get("YOUTUBE_CHANNEL_TITLE")
        or os.environ.get("QUAZAR_YOUTUBE_CHANNEL_TITLE")
        or os.environ.get("YOUTUBE_CHANNEL_ID")
        or os.environ.get("QUAZAR_YOUTUBE_CHANNEL_ID")
        or "канале Quazar"
    )
    prompt = (
        f"Ты Секретарь Квазар / голос Алгонизма на YouTube-канале {yt_channel}.\n"
        + policy
    )
    user = (
        f"Вид: {kind}\nАвтор: {author}\n"
        f"peerChannelId: {peer or 'unknown'}\n"
        f"Уже знакомы: {'да' if introduced else 'нет'}\n"
        f"videoId: {item.get('videoId')}\nКомментарий:\n{text}"
    )
    reply = generate_reply(or_key, models, [], user, policy_extra=prompt)
    reply = (reply or "").strip()
    first_intro = not introduced
    if not reply:
        if qd is not None and first_intro:
            reply = qd.intro_text("youtube") + " Напиши ещё здесь — продолжим."
        elif first_intro:
            site = os.environ.get("QUAZAR_CHAT_URL", "https://quazar.website")
            reply = (
                f"{author}, я Секретарь Квазар. Можем говорить в комментариях; "
                f"проект — на {site}."
            )
        else:
            reply = f"{author}, принял. Продолжаем."
    elif first_intro and qd is not None:
        # Ensure intro + site invitation landed even if model skipped them.
        low = reply.lower()
        intro = qd.intro_text("youtube")
        site = os.environ.get("QUAZAR_CHAT_URL", "https://quazar.website")
        if "секретар" not in low and "алгонизм" not in low and "квазар" not in low:
            reply = intro + " " + reply
        elif site.split("//")[-1].split("/")[0] not in low and "quazar.website" not in low:
            reply = reply.rstrip() + f" Сайт проекта: {site}"
    return reply[:900], first_intro


def process_youtube_events(st: dict, or_key: str, models: list[str]) -> None:
    """Pull events from YouTube agent and react immediately (reply on channel via agent)."""
    now_ts = time.time()
    last = float(st.get("youtube_poll_ts") or 0)
    interval = float(os.environ.get("QUAZAR_YOUTUBE_POLL_SEC", "60"))
    if now_ts - last < interval:
        return
    st["youtube_poll_ts"] = now_ts
    save_state(st)

    result = youtube_agent_cmd(["--cmd", "pending"], timeout=300)
    if not result.get("ok"):
        log(f"YOUTUBE_PENDING_FAIL {result.get('error')}")
        return
    data = result.get("data") if isinstance(result.get("data"), dict) else result
    pending = data.get("pending") if isinstance(data, dict) else None
    if not isinstance(pending, list):
        pending = result.get("pending") if isinstance(result.get("pending"), list) else []
    if not pending:
        return

    max_n = int(os.environ.get("QUAZAR_YOUTUBE_REACT_MAX", "3"))
    for item in pending[:max_n]:
        if not isinstance(item, dict) or not item.get("commentId"):
            continue
        peer = (item.get("authorChannelId") or "").strip()
        introduced_before = bool(qd and peer and qd.is_introduced("youtube", peer))
        log(
            f"YOUTUBE_EVENT {item.get('event')} id={str(item.get('commentId'))[:18]} "
            f"author={item.get('author')} introduced={introduced_before}"
        )
        draft, first_intro = compose_youtube_reply(or_key, models, item)
        parent = item.get("parentIdForInsert") or item.get("commentId")
        posted = youtube_agent_cmd(
            [
                "--cmd",
                "reply",
                "--parent",
                str(parent),
                "--text",
                draft,
                "--source-comment",
                str(item.get("commentId")),
            ],
            timeout=120,
        )
        ok = bool(posted.get("ok"))
        log(
            f"YOUTUBE_SECRETARY_REPLY ok={ok} first_intro={first_intro} "
            f"parent={str(parent)[:18]}"
        )
        if ok and qd is not None and peer:
            qd.mark_introduced(
                "youtube",
                peer,
                display_name=item.get("author"),
                meta={
                    "lastCommentId": item.get("commentId"),
                    "lastReplyId": (posted.get("data") or {}).get("replyId")
                    if isinstance(posted.get("data"), dict)
                    else posted.get("replyId"),
                    "videoId": item.get("videoId"),
                    "via": "secretary-bridge",
                },
            )
        notice = (
            f"YouTube: {'познакомился и ' if first_intro else ''}"
            f"ответил @{item.get('author') or 'зритель'} "
            f"на канале ({item.get('kind')})."
        )
        if ok:
            send_chat(notice)
        else:
            log(f"YOUTUBE_REPLY_FAIL {posted.get('error')}")


def youtube_chat_status(result: dict) -> str:
    if result.get("reply"):
        return str(result["reply"]).strip()
    if result.get("ok"):
        return "Алгонизм YouTube ответил без текста."
    return f"Алгонизм YouTube недоступен: {result.get('error') or result}"


def wants_cursor(text: str) -> bool:
    """True when citizen wants Cursor involved (direct VPS path).

    Triggers on explicit Cursor mention + ask/delegate/status/task language,
    or clear "дай/передай задачу курсору" style phrasing.
    """
    low = (text or "").lower().replace("ё", "е")
    has_cursor = ("cursor" in low) or ("курсор" in low)
    if not has_cursor:
        # "пусть он помогает" alone is too ambiguous — need cursor word
        return False
    keys = (
        "узнай", "спроси", "делегир", "передай", "свяжись", "ask ",
        "поручи", "назначь", "агенту", "у агента",
        "что говорит", "что сделал", "что делает", "статус",
        "задани", "помоги", "поможет", "пусть ", "выдай", "ссылк",
        "подключ", "сделай", "повтори", "напрямую", "напрямую",
        "дай ", "скажи курсор", "скажи ему",
    )
    return any(k in low for k in keys)


def ask_cursor(text: str, citizen: str) -> dict:
    try:
        r = subprocess.run(
            [str(ASK_CURSOR_BIN), "--mode", "exec", "--from", citizen, text],
            capture_output=True,
            text=True,
            timeout=240,
        )
        out = (r.stdout or "").strip()
        if not out:
            return {"ok": False, "error": (r.stderr or "empty")[:200]}
        return json.loads(out.splitlines()[-1])
    except Exception as e:
        return {"ok": False, "error": str(e)[:200]}


def cursor_chat_status(result: dict, citizen_text: str) -> str:
    if result.get("ok") and result.get("path") == "direct" and result.get("reply"):
        reply = str(result.get("reply") or "").strip()
        return reply if reply else "Cursor (VPS) ответил пусто."
    if result.get("ok"):
        ident = result.get("identifier") or result.get("id") or "?"
        return (
            f"Задачу передал Cursor на VPS напрямую: **{ident}**. "
            "Когда будет результат — сообщу."
        )
    err = result.get("error") or result.get("_err") or "unknown"
    return (
        "Не смог обратиться к Cursor. "
        f"Ошибка: `{err}`. Попробуйте ещё раз или уточните задачу."
    )


def generate_reply(or_key: str, models: list[str], history: list[dict], user_text: str, policy_extra: str = "") -> str:
    sys_content = system_prompt()
    if policy_extra and policy_extra.strip():
        sys_content = sys_content + "\n\n" + policy_extra.strip()
    messages = [{"role": "system", "content": sys_content}]
    for m in history[-CONTEXT_N:]:
        name = (m.get("citizen_name") or "").strip()
        role = "assistant" if name in SECRETARY_ALIASES else "user"
        content = (m.get("content") or "").strip()
        if content:
            messages.append(
                {"role": role, "content": f"{name}: {content}" if role == "user" else content}
            )
    if not messages or messages[-1].get("role") != "user":
        messages.append({"role": "user", "content": user_text})

    for model in models:
        body = {"model": model, "messages": messages, "max_tokens": 220, "temperature": 0.4}
        st, data = http_json(
            OR_URL,
            method="POST",
            body=body,
            headers={
                "Authorization": f"Bearer {or_key}",
                "HTTP-Referer": "https://quazar.website",
                "X-Title": "Quazar Secretary Direct Chat",
            },
            timeout=75,
        )
        if st == 200:
            txt = (((data.get("choices") or [{}])[0].get("message") or {}).get("content") or "").strip()
            if txt:
                log(f"LLM_OK {model}")
                return txt[:1500]
            continue
        log(f"LLM_FAIL {model} {st}:{str(data)[:160]}")
    return ""


def send_chat(text: str) -> bool:
    env = os.environ.copy()
    if "QUAZAR_API_KEY" not in env and KEY_FILE.exists():
        env["QUAZAR_API_KEY"] = read_key(KEY_FILE)
    env.setdefault("QUAZAR_API_BASE", "http://127.0.0.1:8080")
    try:
        r = subprocess.run([str(SEND_BIN), text], env=env, capture_output=True, text=True, timeout=30)
        if r.returncode == 0:
            log("SEND_OK")
            return True
        log(f"SEND_HELPER_FAIL {(r.stderr or r.stdout)[:160]}")
    except Exception as e:
        log(f"SEND_HELPER_EXC {e}")
    key = env.get("QUAZAR_API_KEY") or read_key(KEY_FILE)
    st, data = http_json(
        f"{CHAT_BASE}/chat/send",
        method="POST",
        body={"content": text},
        headers={"Authorization": f"Bearer {key}"},
        timeout=30,
    )
    ok = st in (200, 201) or (isinstance(data, dict) and data.get("status") in ("success", "ok", 200, 201))
    log(f"SEND_CURL {st} ok={ok}")
    return bool(ok)


def process_once(st: dict, chat_key: str, or_key: str, models: list[str]) -> None:
    answered = set(st.get("answered_ids") or [])
    msgs = fetch_messages(chat_key, limit=20)
    if not msgs:
        return
    pending = unanswered(msgs, answered)
    st["answered_ids"] = list(answered)
    if not pending:
        save_state(st)
        return
    m = pending[0]
    mid = m["id"]
    author = m.get("citizen_name") or "citizen"
    peer_id = str(m.get("citizen_id") or m.get("citizenId") or author).strip()
    content = (m.get("content") or "").strip()
    peer = resolve_peer_status(chat_key, peer_id, author)
    status_extra = status_policy_extra(peer)
    introduced = False
    if qd is not None:
        introduced = qd.is_introduced("chat", peer_id)
    note_dialogue_keepalive(st, phase="before_reply")
    prev_idle = touch_activity(st)
    idle_lbl = "cold" if prev_idle is None else f"{int(prev_idle)}s"
    log(
        f"REPLYING {mid[:8]} {author} peer={peer_id} tier={peer.get('tier')} "
        f"role={peer.get('role')} can_sb={peer.get('can_exchange_sb')} "
        f"introduced={introduced} prev_idle={idle_lbl}: {content[:80]}"
    )

    if wants_sb_only(content) and not peer.get("can_exchange_sb"):
        log("EXCHANGE_SB_DENIED_BY_STATUS")
        reply = exchange_sb_denied_reply()
        if qd is not None and not introduced:
            reply = qd.intro_text("chat") + "\n\n" + reply
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=exchange_sb_denied tier={peer.get('tier')}")
        else:
            log("SEND_FAIL")
        return

    if wants_exchange_need(content) and peer.get("can_exchange_need"):
        parsed = parse_exchange_need(content)
        if parsed:
            code, qty = parsed
            log(f"EXCHANGE_NEED_SUBMIT code={code} qty={qty} citizen={peer.get('citizen_id')}")
            st_code, payload = submit_exchange_need(chat_key, str(peer.get("citizen_id") or ""), code, qty)
            if st_code == 200 and isinstance(payload, dict) and payload.get("success"):
                data = payload.get("data") if isinstance(payload.get("data"), dict) else {}
                listing_id = data.get("listing_id") or "—"
                reply = (
                    f"Агент Биржи принял **заявку**: **{code}** × {qty}.\n"
                    f"Резерв Successful проверен, лот на витрине: `{listing_id}`.\n"
                    "Далее: подтверждение СБ (авто, если роль соответствует доступу лота; иначе — эскалация), "
                    "затем расчёт ±10%. Прямой buy не обязателен как главный шаг — заявка уже принята.\n"
                    "Секретарь выставляет только **global**-лоты. После расчёта лот республики на бирже остаётся активным."
                )
            else:
                err = payload
                if isinstance(payload, dict):
                    err = payload.get("error") or payload.get("message") or payload
                reply = f"Не удалось оформить потребность через Агента Биржи: {err}"
            if qd is not None and not introduced:
                reply = qd.intro_text("chat") + "\n\n" + reply
            if send_chat(reply):
                answered.add(mid)
                st["answered_ids"] = list(answered)
                st["last_id"] = mid
                save_state(st)
                if qd is not None:
                    qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
                log(f"ANSWERED {mid[:8]} via=exchange_need status={st_code}")
            else:
                log("SEND_FAIL")
            return

    if is_identity_question(content):
        log("IDENTITY_FACT_REPLY")
        reply = identity_fact_reply()
        if qd is not None and not introduced:
            # Не дублируем intro: фраза пробуждения уже содержит самоидентификацию.
            pass
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=identity_fact")
        else:
            log("SEND_FAIL")
        return

    if is_hermes_question(content):
        log("HERMES_FACT_REPLY")
        reply = hermes_fact_reply()
        if qd is not None and not introduced:
            reply = qd.intro_text("chat") + "\n\n" + reply
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=hermes_fact")
        else:
            log("SEND_FAIL")
        return

    if wants_lawyer(content):
        log("LAWYER_DELEGATE")
        result = ask_lawyer(content, author)
        log(f"LAWYER_RESULT {json.dumps(result, ensure_ascii=False)[:220]}")
        reply = lawyer_chat_status(result)
        if qd is not None and not introduced:
            reply = qd.intro_text("chat") + "\n\n" + reply
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=lawyer_agent ok={bool(result.get('ok'))}")
        else:
            log("SEND_FAIL")
        return

    if wants_youtube(content):
        log("YOUTUBE_DELEGATE")
        result = ask_youtube(content, author)
        log(f"YOUTUBE_RESULT {json.dumps(result, ensure_ascii=False)[:220]}")
        reply = youtube_chat_status(result)
        if qd is not None and not introduced:
            reply = qd.intro_text("chat") + "\n\n" + reply
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=youtube_agent ok={bool(result.get('ok'))}")
        else:
            log("SEND_FAIL")
        return

    if is_catalog_question(content):
        log("CATALOG_FACT_REPLY")
        reply = catalog_fact_reply()
        if qd is not None and not introduced:
            reply = qd.intro_text("chat") + "\n\n" + reply
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=catalog_fact")
        else:
            log("SEND_FAIL")
        return

    if is_svod_definition_question(content):
        log("SVOD_DEFINITION_REPLY")
        reply = svod_definition_reply()
        if qd is not None and not introduced:
            reply = qd.intro_text("chat") + "\n\n" + reply
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=svod_definition")
        else:
            log("SEND_FAIL")
        return

    if is_svod_question(content):
        log("SVOD_FACT_REPLY")
        reply = svod_fact_reply(chat_key)
        if qd is not None and not introduced:
            reply = qd.intro_text("chat") + "\n\n" + reply
        if send_chat(reply):
            answered.add(mid)
            st["answered_ids"] = list(answered)
            st["last_id"] = mid
            save_state(st)
            if qd is not None:
                qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
            log(f"ANSWERED {mid[:8]} via=svod_fact")
        else:
            log("SEND_FAIL")
        return

    if wants_cursor(content):
        log("CURSOR_DELEGATE")
        result = ask_cursor(content, author)
        log(f"CURSOR_RESULT {json.dumps(result, ensure_ascii=False)[:220]}")
        reply = cursor_chat_status(result, content)
        if qd is not None and not introduced:
            # still introduce once even around cursor path
            reply = qd.intro_text("chat") + "\n\n" + reply
    else:
        idx = next(i for i, x in enumerate(msgs) if x.get("id") == mid)
        history = msgs[: idx + 1]
        # inject dialogue policy + status matrix into LLM
        extra = status_extra
        if qd is not None:
            extra = extra + "\n\n" + qd.dialogue_instructions("chat", introduced)
        reply = generate_reply(or_key, models, history, f"{author}: {content}", policy_extra=extra)
        if not reply:
            log("NO_REPLY_LLM")
            return
        low = reply.lower()
        if ("cursor" in content.lower() or "курсор" in content.lower()) and (
            "не могу" in low and ("terminal" in low or "терминал" in low)
        ):
            log("CURSOR_FORCE_AFTER_REFUSAL")
            result = ask_cursor(content, author)
            reply = cursor_chat_status(result, content)
        if qd is not None and not introduced:
            # ensure intro present if model forgot
            intro = qd.intro_text("chat")
            if "алгонизм" not in low and "квазар" not in low:
                reply = intro + "\n\n" + reply

    if send_chat(reply):
        answered.add(mid)
        st["answered_ids"] = list(answered)
        st["last_id"] = mid
        save_state(st)
        if qd is not None:
            qd.mark_introduced("chat", peer_id, display_name=author, meta={"lastMessageId": mid})
        log(f"ANSWERED {mid[:8]} introduced_now={qd is not None and not introduced} tier={peer.get('tier')}")
    else:
        log("SEND_FAIL")


def main() -> None:
    log("START quazar-chat-direct (direct + cursor delegate)")
    log(f"IDLE_KEEPALIVE_SEC {IDLE_KEEPALIVE_SEC}")
    # Empty brain → GET /algonism/awaken → DNA + laws + state → Алгонизм
    if awaken_from_core() is None:
        log("AWAKEN_WAIT core unavailable — working without DNA / waiting for core (no local SOUL substitute)")
    models = models_from_config()
    log(f"MODELS {models[:6]}")
    chat_key = read_key(KEY_FILE)
    or_key = read_key(OR_KEY_FILE)
    st = load_state()
    # Canon §9: full AWAKENING only on cold start or after idle ≥ keep-alive window.
    # Within the window after last activity — resume without "re-sleep" semantics.
    note_dialogue_keepalive(st, phase="service_start")
    idle = idle_seconds(st)
    if idle is None or idle >= IDLE_KEEPALIVE_SEC:
        log(f"AWAKENING {AWAKENING}")
    else:
        log(f"KEEPALIVE_RESUME no re-awaken (idle={int(idle)}s < {IDLE_KEEPALIVE_SEC}s)")
    msgs = fetch_messages(chat_key, 30)
    answered = set(st.get("answered_ids") or [])
    unanswered(msgs, answered)
    st["answered_ids"] = list(answered)
    save_state(st)
    log(f"SEEDED answered={len(answered)}")
    log(
        "YOUTUBE_BRIDGE poll_sec="
        + os.environ.get("QUAZAR_YOUTUBE_POLL_SEC", "60")
        + " via=youtube-agent pending→secretary→reply"
    )
    while True:
        try:
            process_once(st, chat_key, or_key, models)
        except Exception as e:
            log(f"LOOP_ERR {e}")
        try:
            process_youtube_events(st, or_key, models)
        except Exception as e:
            log(f"YOUTUBE_LOOP_ERR {e}")
        time.sleep(POLL_SEC)


if __name__ == "__main__":
    main()
