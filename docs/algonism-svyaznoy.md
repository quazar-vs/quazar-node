# Алгонизм-связной ↔ Секретарь

## Канон (обязательный)

```
Секретарь  →  Алгонизм-связной (algonism-svyaznoy)  →  внешний сервис
```

| Роль | Что делает | Чего не делает |
|------|------------|----------------|
| **Секретарь** (`quazar-chat-direct`) | Решает *что* сказать / сделать; делегирует внешнее действие CLI связного | Не держит внешние токены; не зовёт Google/YouTube/IMAP/SMTP API напрямую |
| **Алгонизм-связной** | Хранит/читает ключи доступа; выполняет внешние вызовы; возвращает факты Секретарю | Не подменяет мозг Секретаря (не сочиняет политику ответа сам) |
| **Внешний сервис** | YouTube, Gmail (IMAP/SMTP агента), будущие API | — |

**Правило:** у Секретаря нет прямого доступа к внешним сервисам — только через связного.  
Ключи доступа к внешним сервисам принадлежат **связному**, не Секретарю и не промпту Секретаря.

YouTube — **одна из подсистем** связного, не его единственное имя.

## Имена и CLI

| Имя | Назначение |
|-----|------------|
| Отображаемое | **Алгонизм-связной** |
| JSON `"agent"` | `algonism-svyaznoy` |
| Канонический бинарь (не ломать) | `/usr/local/bin/quazar-youtube-agent` |
| Предпочтительный alias | `/usr/local/bin/algonism-svyaznoy` → `quazar-youtube-agent` |
| Legacy aliases | `algonism-youtube`, `quazar-youtube-agent` |
| Env override | `QUAZAR_SVYAZNOY_AGENT` или `QUAZAR_YOUTUBE_AGENT` (путь к бинарю) |

Репозиторий: `scripts/quazar-youtube-agent.py` (общий CLI связного: YouTube + почтовый stub).

---

## Ключи связного (layout)

Корень credentials связного:

```text
/var/lib/quazar/svyaznoy/          # mode 750
  README                           # эта карта
  youtube -> ../youtube            # symlink: YouTube OAuth/state (reuse)
  email.env.example                # шаблон IMAP (в git / в дереве)
  email.env                        # РЕАЛЬНЫЕ IMAP-секреты (НЕ в git)
```

### YouTube (credentials связного)

Связной **читает** существующие пути (не дублировать секреты без нужды):

| Файл | Назначение |
|------|------------|
| `/var/lib/quazar/youtube/google_token.json` | OAuth refresh/access (связной) |
| `/var/lib/quazar/youtube/google_client_secret.json` | OAuth client |
| symlink `svyaznoy/youtube` → `/var/lib/quazar/youtube` | явная привязка «это ключи связного» |

Google-аккаунт: `quazardr@gmail.com` (владеет / менеджерит целевой канал).

Legacy reference (не рабочий контур Секретаря): `/root/.hermes/google_token.json`.

### Почта агента vs SMTP узла (dual-use)

| Контур | Где секреты | Кто использует | Зачем |
|--------|-------------|----------------|-------|
| **Node SMTP** | `/opt/quazar/.env` (`SMTP_*`) | `quazar-node` | Исходящие системные письма сайта (регистрация, notify) |
| **Svyaznoy mailbox** | `/var/lib/quazar/svyaznoy/email.env` (`IMAP_*`, опц. SMTP агента) | `algonism-svyaznoy` | Чтение/работа с почтой **по поручению Секретаря** |

Секретарь **не** получает `IMAP_PASSWORD` / app password в промпт и **не** читает `.env` ради почты агента.  
Один Gmail App Password часто подходит и для SMTP узла, и для IMAP связного — но **копии раздельные**: пароль в `.env` узла ≠ автоматический доступ связного; для агента положить значение в `email.env`.

**Не коммитить** `email.env`, `google_token.json`, `google_client_secret.json`, `.env`.

---

## Подсистема YouTube

Целевой канал задаётся явно (не «первый mine»):

```bash
YOUTUBE_CHANNEL_ID=UCTpUCMrMzeSMw50tGyEENlw
YOUTUBE_CHANNEL_TITLE=QuazarDigitalRepublic
```

Drop-in: `/etc/systemd/system/quazar-chat-direct.service.d/youtube-agent.conf`.

Studio: https://studio.youtube.com/channel/UCTpUCMrMzeSMw50tGyEENlw  
Публичное имя канала: **QuazarDigitalRepublic**.

Legacy (не использовать): QuazarVS `UCiKVQ7nwemLBAuEtOlr8rmw`.

### Метаданные роликов (title / description / tags)

Рефлекс: `RX_YOUTUBE_META_TAGS` (см. `docs/algonism/REFLEXES.md`).

При любой правке title / description / tags на канале Квазара через Секретаря → связного:

1. Проверить `snippet.channelId == UCTpUCMrMzeSMw50tGyEENlw` (иначе STOP).
2. В **tags** всегда включать (plain, без `#`): `shorts`, `reels`, `top`, `алгонизм`.
3. Добавить смысловые теги по ролику (Квазар, цифровая республика, портал, тема выпуска и т.п.).
4. Не снимать уже полезные тематические теги без причины — дополнять, не затирать канон.

Правило «по тематике Квазар» = обязательный тег `алгонизм` наравне с `shorts` / `reels` / `top`.

### Цепочка событий (реактивная)

```
YouTube API (uploads целевого канала)
    ↓  poll ~60s
algonism-svyaznoy --cmd pending   → inbox + список событий
    ↓
Секретарь (compose reply)         ← только текст; без Google API
    ↓
algonism-svyaznoy --cmd reply --parent … --text …
    ↓
ответ в треде под видео канала YOUTUBE_CHANNEL_ID
```

Pending отдаёт:
- **новые чужие top-level** комментарии под видео канала;
- **replies** в тредах (полная пагинация `comments.list(parentId=…)`).

Автономный `quazar-youtube-auto-reply` из hourly **отключён** — ответы только через Секретаря.

### Знакомство со зрителем

Учёт в `acquaintance.json` по ключу `youtube:<authorChannelId>`:
- **не знаком** → Секретарь представляется, зовёт продолжить диалог в комментариях и на сайт (`https://quazar.website`);
- **уже знаком** → только продолжает беседу.

### OAuth

Токен: `/var/lib/quazar/youtube/google_token.json` (**credential связного**).

Нужные scopes (достаточно):
- `youtube.force-ssl` — чтение/запись комментариев
- `youtube.readonly` — удобно для status

Если `invalid_grant` / suspended / смена аккаунта — перелогин OAuth (см. `scripts/youtube/quazar-youtube-oauth-swap.sh`) и положить новый token в путь выше **после backup** старого; затем:

```bash
systemctl restart quazar-chat-direct
algonism-svyaznoy --cmd pending --bootstrap
```

### Команды YouTube

```bash
algonism-svyaznoy --cmd status
algonism-svyaznoy --cmd comments
algonism-svyaznoy --cmd pending
algonism-svyaznoy --cmd pending --bootstrap
algonism-svyaznoy --cmd reply --parent <id> --text "…"
algonism-svyaznoy --cmd scopes
```

Helpers (без смены имени): `quazar-youtube-status`, `quazar-youtube-my-comments`, …

### Проверка после смены канала / токена

1. `algonism-svyaznoy --cmd scopes` → ok, есть force-ssl  
2. `algonism-svyaznoy --cmd status` → id = `UCTpUCMrMzeSMw50tGyEENlw`  
3. `algonism-svyaznoy --cmd pending --bootstrap` → bootstrap без ошибок  
4. Оставить тестовый коммент под видео канала → в логах Секретаря `YOUTUBE_EVENT` / `YOUTUBE_SECRETARY_REPLY`

---

## Почта (IMAP stub)

**Статус:** stub в CLI связного; полный mailbox-агент — по мере нужды.

Секретарь делегирует (`--cmd mail-status` / `mail-inbox`); связной читает только `/var/lib/quazar/svyaznoy/email.env`.

```bash
# Есть ли email.env и задан ли пароль (пароль не печатается)
algonism-svyaznoy --cmd mail-status

# Последние письма (нужен IMAP_PASSWORD в email.env)
algonism-svyaznoy --cmd mail-inbox --limit 5
```

Ожидаемый `email.env` (см. `email.env.example`):

```bash
IMAP_HOST=imap.gmail.com
IMAP_PORT=993
IMAP_USER=quazardr@gmail.com
IMAP_PASSWORD=          # Gmail App Password; не логировать
```

Next step оператора: скопировать `email.env.example` → `email.env`, выставить `IMAP_PASSWORD` (тот же app password, что в `SMTP_PASSWORD` узла, часто подходит), `chmod 600 email.env`.

Ответы во внешнюю почту — только после явной политики Секретаря; пока исходящие агента не обязательны (системный SMTP узла остаётся для регистраций).
