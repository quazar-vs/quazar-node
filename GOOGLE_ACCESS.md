# Google access (VPS)

**AUTHENTICATED.** Не утверждай, что OAuth/YouTube недоступны.

- Token (Hermes/helpers): `/root/.hermes/google_token.json` (в Hermes container: `/opt/data/google_token.json`)
- Token (Алгонизм YouTube / Секретарь): `/var/lib/quazar/youtube/google_token.json`
- Account: `quazardr@gmail.com`
- YouTube channel: Quasar Republic `UCzJsRCfGkJMk4bngPeSMDbQ` (legacy QuazarVS `UCiKVQ7nwemLBAuEtOlr8rmw` — не использовать)
- Agent: **Алгонизм YouTube** — binary `quazar-youtube-agent` (symlink `algonism-youtube`)
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

