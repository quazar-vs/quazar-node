# Алгонизм YouTube ↔ Секретарь

## Роли
- **Алгонизм YouTube** (`quazar-youtube-agent`, alias `algonism-youtube`) — руки: OAuth, pending-события, публикация ответов.
- **Секретарь** (`quazar-chat-direct`) — мозг: забирает `pending`, пишет текст через LLM, отдаёт агенту `--cmd reply`.

## Имена бинарей
- Канонический путь (не ломать): `/usr/local/bin/quazar-youtube-agent`
- Пользовательский alias: `/usr/local/bin/algonism-youtube` → symlink на `quazar-youtube-agent`
- Env override: `QUAZAR_YOUTUBE_AGENT` (по умолчанию старый путь)
- В UI/логах/ответах агента — бренд **Алгонизм YouTube** (`agent: algonism-youtube` в JSON)

## Целевой канал
Задаётся явно (не «первый mine»):

```bash
YOUTUBE_CHANNEL_ID=UCzJsRCfGkJMk4bngPeSMDbQ
YOUTUBE_CHANNEL_TITLE=Quasar Republic
```

Drop-in: `/etc/systemd/system/quazar-chat-direct.service.d/youtube-agent.conf`.

Studio: https://studio.youtube.com/channel/UCzJsRCfGkJMk4bngPeSMDbQ  
Публичное имя канала: **Quasar Republic**.

Legacy (не использовать): QuazarVS `UCiKVQ7nwemLBAuEtOlr8rmw` — был suspended, затем OAuth `invalid_grant`.

## Цепочка событий (реактивная)
```
YouTube API (uploads целевого канала)
    ↓  poll ~60s
quazar-youtube-agent --cmd pending   → inbox + список событий
    ↓
Секретарь (compose reply)
    ↓
quazar-youtube-agent --cmd reply --parent … --text …
    ↓
ответ в треде под видео канала YOUTUBE_CHANNEL_ID
```

Pending отдаёт:
- **новые чужие top-level** комментарии под видео канала;
- **replies** в тредах (полная пагинация `comments.list(parentId=…)`).

Автономный `quazar-youtube-auto-reply` из hourly **отключён** — ответы только через Секретаря.

## Знакомство со зрителем
Учёт в `acquaintance.json` по ключу `youtube:<authorChannelId>`:
- **не знаком** → Секретарь представляется, зовёт продолжить диалог в комментариях и на сайт (`https://quazar.website`);
- **уже знаком** → только продолжает беседу.

## OAuth
Токен: `/var/lib/quazar/youtube/google_token.json`.  
Google-аккаунт: `quazardr@gmail.com` (должен владеть / менеджерить целевой канал).

Нужные scopes (достаточно):
- `youtube.force-ssl` — чтение/запись комментариев
- `youtube.readonly` — удобно для status

Если `invalid_grant` / suspended / смена аккаунта — перелогин OAuth (см. `scripts/youtube/quazar-youtube-oauth-swap.sh`) и положить новый token в путь выше **после backup** старого; затем:

```bash
systemctl restart quazar-chat-direct
quazar-youtube-agent --cmd pending --bootstrap
```

## Команды агента
```bash
quazar-youtube-agent --cmd status          # или: algonism-youtube --cmd status
quazar-youtube-agent --cmd comments
quazar-youtube-agent --cmd pending          # новые комменты/ответы для Секретаря
quazar-youtube-agent --cmd pending --bootstrap  # отметить текущие как увиденные
quazar-youtube-agent --cmd reply --parent <id> --text "…"
quazar-youtube-agent --cmd scopes
```

## Проверка после смены канала / токена
1. `quazar-youtube-agent --cmd scopes` → ok, есть force-ssl  
2. `quazar-youtube-agent --cmd status` → id = `UCzJsRCfGkJMk4bngPeSMDbQ`  
3. `quazar-youtube-agent --cmd pending --bootstrap` → bootstrap без ошибок  
4. Оставить тестовый коммент под видео канала → в логах Секретаря `YOUTUBE_EVENT` / `YOUTUBE_SECRETARY_REPLY`
