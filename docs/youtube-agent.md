# Агент YouTube ↔ Секретарь

## Роли
- **Агент YouTube** (`quazar-youtube-agent`) — руки: OAuth, pending-события, публикация ответов, максимум доступных scopes канала.
- **Секретарь** (`quazar-chat-direct`) — мозг: забирает `pending`, пишет текст через LLM, отдаёт агенту `--cmd reply`.

## Цепочка событий (реактивная)
```
YouTube API
    ↓  poll ~60s
quazar-youtube-agent --cmd pending   → inbox + список событий
    ↓
Секретарь (compose reply)
    ↓
quazar-youtube-agent --cmd reply --parent … --text …
    ↓
ответ под комментарием на канале QuazarVS
```

Автономный `quazar-youtube-auto-reply` из hourly **отключён** — ответы только через Секретаря.

## Знакомство со зрителем
Учёт в `acquaintance.json` по ключу `youtube:<authorChannelId>`:
- **не знаком** → Секретарь представляется, зовёт продолжить диалог в комментариях и на сайт проекта (`https://quazar.website`);
- **уже знаком** → только продолжает беседу, без повторного представления.

## Команды агента
```bash
quazar-youtube-agent --cmd status
quazar-youtube-agent --cmd comments
quazar-youtube-agent --cmd pending          # новые комменты/ответы для Секретаря
quazar-youtube-agent --cmd pending --bootstrap  # отметить текущие как увиденные
quazar-youtube-agent --cmd reply --parent <id> --text "…"
quazar-youtube-agent --cmd scopes           # фактические OAuth scopes
quazar-youtube-agent "сколько подписчиков на ютуб"
```

## Доступ к каналу
Токен: `/var/lib/quazar/youtube/google_token.json`.

Желательные scopes (агент запрашивает максимально полезный набор):
- `youtube.force-ssl` — чтение/запись комментариев (достаточно для ответов)
- `youtube.readonly`
- `youtubepartner` (если выдан)

Проверка: `quazar-youtube-agent --cmd scopes`.

## Чат-гражданин
- «статус YouTube / канала»
- «мои комментарии»
- «что нового на ютуб» / pending
- «scopes / доступ агента»

Секретарь не выдумывает цифры — делегирует агенту.
