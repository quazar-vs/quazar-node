# Handoff: YouTube-контур Секретаря на Rust

*Промпт на будущее. Не делать сейчас — только когда явно попросят выполнить этот handoff.*

Скопируй блок ниже в новый чат агента на VPS `/opt/quazar`.

---

## Промпт

```
Задача: перенести YouTube-контур Секретаря с Python на Rust (один стек с quazar-node).

Контекст (состояние на 2026-08-14):
- Секретарь: systemd `quazar-chat-direct.service` → `/usr/local/bin/quazar-chat-direct.py`
  (репо: `scripts/quazar-chat-direct.py`). Poll ~60с → `quazar-youtube-agent --cmd pending`
  → LLM-ответ → `--cmd reply`. Знакомство: `quazar_dialogue` + `/root/.hermes/acquaintance.json`
  ключ `youtube:<authorChannelId>`.
- Агент YouTube: `/usr/local/bin/quazar-youtube-agent` (репо: `scripts/quazar-youtube-agent.py`),
  venv `/var/lib/quazar/venv-youtube`, токен `/var/lib/quazar/youtube/google_token.json`.
  Команды: status|comments|pending|reply|scopes|watchlist|sync-likes.
  Pending: только uploads канала QuazarVS (до 30 видео), чужие комменты/ответы; bootstrap
  отмечает старые. State: `/var/lib/quazar/youtube/` (inbox + youtube_secretary_bridge_state.json).
- Hourly: `/usr/local/bin/quazar-youtube-hourly.sh` — БЕЗ auto-reply (ответы только через Секретаря).
- Документация: `docs/youtube-agent.md`, рефлекс `RX_YOUTUBE_EVENT_REACT` в `docs/algonism/REFLEXES.md`.
- Ограничение API: WebSub/PubSubHubbub пушит только новое видео / title / description;
  на комментарии push НЕТ — только poll (или оставить poll в Rust).
- OAuth scopes: youtube.force-ssl (+ readonly); force-ssl достаточно для reply.
  Аккаунт: quazarvs@gmail.com / канал UCiKVQ7nwemLBAuEtOlr8rmw.
- Узел: Rust `quazar-node` в `/opt/quazar` — YouTube-модуля пока нет.
- Passport Flutter на VPS НЕ трогать.

Цель:
1. Новый Rust-бинарь (предпочтительно `src/bin/youtube_agent.rs` или crate `quazar-youtube`) с CLI,
   совместимым по смыслу с текущим агентом:
   --cmd pending [--bootstrap] | reply --parent --text [--source-comment] | scopes | status | comments
   JSON stdout как сейчас (ok/agent/cmd/data/reply), чтобы Секретарь мог переключиться без ломки.
2. OAuth: читать/обновлять существующий `google_token.json` (не ломать Python-helpers, пока они живы).
3. Перенести (или разделить этапы):
   A) руки агента: pending/reply/scopes на Rust;
   B) мозг: цикл «pending → acquaintance → OpenRouter → reply» на Rust
      (замена youtube-части chat-direct) ИЛИ оставить LLM в chat-direct, но звать Rust-агента.
4. Знакомство: та же семантика — не знаком → intro + приглашение в комментарии + сайт
   https://quazar.website; знаком → только продолжение беседы. Хранилище совместимо с
   acquaintance.json (ключ youtube:<channelId>) или явная миграция + dual-read.
5. Задеплоить: systemd unit(s), smoke scopes/pending/reply, логи; выключить/оставить Python
   как fallback только по флагу, не два автоответчика сразу.
6. Обновить `docs/youtube-agent.md` и REFLEXES при смене бинарей.

Не делать в этом же прогоне без отдельной просьбы:
- WebSub webhook (отдельная задача);
- удаление /root/.hermes;
- force-push / правки Passport app.

Критерии приёмки:
- `quazar-youtube-agent --cmd scopes` (Rust) → ok, force-ssl виден;
- `--cmd pending` после bootstrap не спамит старыми; новый коммент → событие;
- Секретарь (Rust или Python→Rust) постит reply; first contact = intro+сайт+диалог в комментариях;
  повторный контакт того же authorChannelId = без повторного intro;
- hourly по-прежнему без автономного auto-reply;
- нет двойной публикации с Python auto-reply / старым bridge.

Порядок работ:
1) Инвентаризация текущего Python CLI и JSON-контракта.
2) Rust MVP: scopes + pending + reply на том же токене.
3) Проводка Секретаря на Rust-бинарь.
4) Acquaintance + политика intro.
5) Deploy, smoke на живом канале (осторожно с тестовым комментом).
6) Документация + опционально deprecate Python-агента.

Стек: Rust edition как в Cargo.toml узла; HTTP-клиент уже принятый в проекте;
секреты не коммитить. Коммит/push — только если попросят.
```

---

## Краткие ссылки

| Что | Где |
|-----|-----|
| Секретарь | `scripts/quazar-chat-direct.py` → `process_youtube_events` / `compose_youtube_reply` |
| Агент | `scripts/quazar-youtube-agent.py` |
| Диалог | `scripts/youtube/quazar_dialogue.py` |
| Док | `docs/youtube-agent.md` |
| Токен | `/var/lib/quazar/youtube/google_token.json` |
| Acquaintance | `/root/.hermes/acquaintance.json` |
