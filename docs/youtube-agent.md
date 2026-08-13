# Агент YouTube ↔ Секретарь

## Цепочка
Citizen-чат → **Секретарь** (`quazar-chat-direct`) → **`quazar-youtube-agent`** → helpers (`status` / `comments` / …) → **YouTube API** (OAuth в `/var/lib/quazar/youtube/google_token.json`).

## Команды агента
```bash
quazar-youtube-agent --cmd status
quazar-youtube-agent --cmd comments
quazar-youtube-agent "сколько подписчиков на ютуб"
```

## Что спрашивать в чате
- «статус YouTube / канала»
- «мои комментарии на ютуб»
- «агент ютуб»
- «сколько подписчиков QuazarVS»

Секретарь не выдумывает цифры — делегирует агенту YouTube.
