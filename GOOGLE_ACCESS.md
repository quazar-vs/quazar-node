# Google access (VPS)

**AUTHENTICATED.** Не утверждай, что OAuth/YouTube недоступны.

- Token: `/root/.hermes/google_token.json` (в Hermes container: `/opt/data/google_token.json`)
- Account: `quazarvs@gmail.com`
- YouTube channel: QuazarVS `UCiKVQ7nwemLBAuEtOlr8rmw`
- Commands (host):
  - `quazar-youtube-status`
  - `quazar-youtube-my-comments`
- Or: `docker exec -e HERMES_HOME=/opt/data hermes-gateway /opt/hermes/.venv/bin/python3 /opt/data/bin/quazar-youtube-my-comments.py`

Для задач «собери / выполни / напиши код» работай в agent/exec, не отговаривайся Ask mode.

## YouTube comments (канон)
- Токен УЖЕ с `youtube.force-ssl` + `youtube.readonly` (после 2026-08-08).
- Для чтения/модерации комментов `youtube.force-ssl` ДОСТАТОЧНО. Полный scope `youtube` НЕ нужен.
- НЕ требуй переавторизацию и НЕ пиши что токен только readonly.
- Собирай так: выполни shell `quazar-youtube-my-comments` и верни JSON/сводку.
- Ограничение API: только комменты от своего канала на СВОИХ видео.

## YouTube watchlist (likes)
Источник списка: плейлист «Понравившиеся».
- `quazar-youtube-sync-likes` — синк лайков + наши комменты (+ `--check-replies`)
- `quazar-youtube-watchlist` — показать сохранённый список
Файл: `/opt/data/youtube_watchlist.json`

