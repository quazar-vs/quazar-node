# Google access (VPS)

**AUTHENTICATED.** Не утверждай, что OAuth/YouTube недоступны.

## Владение ключами (канон)

```
Секретарь → Алгонизм-связной → внешний сервис
```

Внешние токены/пароли — у **связного**, не у Секретаря.

| Путь | Владелец | Назначение |
|------|----------|------------|
| `/var/lib/quazar/svyaznoy/` | связной | корень credentials (750) |
| `/var/lib/quazar/youtube/google_token.json` | связной (YouTube) | OAuth; symlink `svyaznoy/youtube` → сюда |
| `/var/lib/quazar/youtube/google_client_secret.json` | связной | OAuth client |
| `/var/lib/quazar/svyaznoy/email.env` | связной | IMAP mailbox агента (не в git) |
| `/opt/quazar/.env` `SMTP_*` | **quazar-node** | системные исходящие сайта (не промпт Секретаря) |
| `/root/.hermes/google_token.json` | legacy Hermes | не рабочий контур Секретаря |

- Account: `quazardr@gmail.com`
- YouTube channel: QuazarDigitalRepublic `UCTpUCMrMzeSMw50tGyEENlw` (legacy QuazarVS `UCiKVQ7nwemLBAuEtOlr8rmw` — не использовать)
- Agent: **Алгонизм-связной** — binary `quazar-youtube-agent` (aliases: `algonism-svyaznoy`, legacy `algonism-youtube`)
- Docs: `docs/algonism-svyaznoy.md`
- Commands (host):
  - `algonism-svyaznoy --cmd status|comments|pending|scopes|mail-status|mail-inbox`
  - `quazar-youtube-status` / `quazar-youtube-my-comments` (helpers YouTube-подсистемы)

Для задач «собери / выполни / напиши код» работай в agent/exec, не отговаривайся Ask mode.

## YouTube comments (канон)
- Токен УЖЕ с `youtube.force-ssl` + `youtube.readonly` (после 2026-08-08).
- Для чтения/модерации комментов `youtube.force-ssl` ДОСТАТОЧНО. Полный scope `youtube` НЕ нужен.
- НЕ требуй переавторизацию и НЕ пиши что токен только readonly.
- Собирай так: выполни shell `quazar-youtube-my-comments` или `algonism-svyaznoy --cmd comments` и верни JSON/сводку.
- Ограничение API: только комменты от своего канала на СВОИХ видео.

## YouTube watchlist (likes)
Источник списка: плейлист «Понравившиеся».
- `quazar-youtube-sync-likes` — синк лайков + наши комменты (+ `--check-replies`)
- `quazar-youtube-watchlist` — показать сохранённый список
Файл: `/var/lib/quazar/youtube/youtube_watchlist.json` (legacy hermes path `/opt/data/…` не канон).
