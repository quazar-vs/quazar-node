# Архитектура: ядро и внешние агенты (контейнеры)

*Канон разделения контуров. Статус: внедрение начато (stub + compose), прод Секретаря пока на host systemd.*

## Схема

```
┌─────────────────────────── CORE (compose.core / docker-compose) ───────────────────────────┐
│  postgres · quazar-node · (опц.) crypto-agent / lawyer · будущий Secretary-in-core          │
│  сеть Docker: quazar                                                                        │
│  без внешних OAuth/IMAP/Cursor credentials                                                  │
└─────────────────────────────────────────────┬───────────────────────────────────────────────┘
                                              │ HTTP / DNS compose
                                              │ SVYAZNOY_URL · CURSOR_AGENT_URL
┌─────────────────────────────────────────────▼───────────────────────────────────────────────┐
│  EXTERNAL AGENTS (deploy/external-agents · profile external)                                │
│  external-agents stub → /health · /v1/svyaznoy/* · /v1/cursor/*                             │
│  сети: quazar + quazar-external                                                             │
│  ключи связного ТОЛЬКО здесь: /var/lib/quazar/svyaznoy · youtube (mount)                    │
└─────────────────────────────────────────────┬───────────────────────────────────────────────┘
                                              │
                              YouTube / Gmail / Cursor / … (внешний мир)
```

**Решение по связному:** живёт во **внешнем** контуре (не sidecar внутри образа ядра).  
Это согласовано с каноном «внешние ключи у связного» (`docs/algonism-svyaznoy.md`, DNA §4).

Секретарь обращается:

| Цель | Внутренние агенты | Внешние агенты |
|------|-------------------|----------------|
| Где | тот же core / host CLI юриста и т.п. | контейнер `external-agents` |
| Как | процесс/API ядра | HTTP по `SVYAZNOY_URL` / `CURSOR_AGENT_URL` |
| Ключи внешних API | нет | только у связного / Cursor worker |

Цепочка внешних действий не меняется:

```
Секретарь → Алгонизм-связной → внешний сервис
```

Меняется только транспорт до связного: subprocess CLI (сегодня) **или** HTTP в контейнер (целевое).

## Установка

### 1) Только ядро

```bash
cd /opt/quazar
docker compose -f compose.core.yml up -d
# эквивалент текущего продакшена по смыслу:
# docker compose -f docker-compose.yml up -d
```

Поднимает Postgres + `quazar-node`. Внешние агенты **не** стартуют.

### 2) Только / отдельно внешние агенты

```bash
cd /opt/quazar
docker compose -f deploy/external-agents/docker-compose.yml up -d --build
```

Проверка:

```bash
curl -sS http://127.0.0.1:8090/health
curl -sS http://127.0.0.1:8090/v1/agents
```

Порт `127.0.0.1:8090` — мост для **host** Секретаря (`quazar-chat-direct.service`) без переноса в Docker.

### 3) Ядро + внешние (один хост)

```bash
cd /opt/quazar
docker compose -f docker-compose.yml -f compose.external-agents.yml --profile external up -d --build
```

Или два независимых compose (оба объявляют сеть `name: quazar` — присоединяются по имени).

## Что нужно Секретарю для вызова внешнего контейнера

1. Контейнер `external-agents` healthy на `:8090`.
2. Env (systemd drop-in или будущий secretary service):

```bash
# Host Secretary → published port
SVYAZNOY_URL=http://127.0.0.1:8090
CURSOR_AGENT_URL=http://127.0.0.1:8090

# Secretary inside Docker (будущее) → DNS сети quazar
# SVYAZNOY_URL=http://external-agents:8090
# CURSOR_AGENT_URL=http://external-agents:8090
```

3. Контракт stub (совместимый задел):

| Метод | Путь | Назначение |
|-------|------|------------|
| GET | `/health` | liveness |
| GET | `/v1/agents` | каталог |
| POST | `/v1/svyaznoy/cmd` | тело JSON `{ "args": [...], "from": "…" }` |
| POST | `/v1/cursor/exec` | тело JSON `{ "text": "…", "from": "…" }` |

Если `SVYAZNOY_URL` / `CURSOR_AGENT_URL` **не заданы**, `quazar-chat-direct` сохраняет прежний путь: CLI `algonism-svyaznoy` и `quazar-cursor-direct` на хосте (прод не ломается).

## Текущее состояние продакшена (не ломать)

| Компонент | Сейчас | Цель |
|-----------|--------|------|
| `quazar-node` + Postgres | `docker-compose.yml` | core |
| Секретарь | systemd `quazar-chat-direct` | позже — сервис в core |
| Связной / YouTube | host CLI + `/var/lib/quazar/svyaznoy` | HTTP в external-agents + mount ключей |
| Cursor | host `quazar-cursor-direct` | cursor-worker в external-agents |
| Hermes | profile `hermes` (deprecated) | не использовать |

Основной `docker-compose.yml` по-прежнему поднимает только postgres + node на сети `default`, чтобы обычный `up` не пересоздавал прод на новый bridge. Явная сеть `quazar` создаётся overlay/external compose и `compose.core.yml`.

## Файлы

| Путь | Роль |
|------|------|
| `compose.core.yml` | явная установка ядра + сеть `quazar` |
| `docker-compose.yml` | прод-core (additive комментарии / env stubs / объявление сетей) |
| `compose.external-agents.yml` | overlay + profile `external` |
| `deploy/external-agents/` | Dockerfile, stub server, standalone compose |
| `.env` | `SVYAZNOY_URL`, `CURSOR_AGENT_URL` (опционально, без секретов в git) |

## Следующие шаги (не в этом PR-шаге)

1. Реальный HTTP adapter связного (обёртка над `quazar-youtube-agent`) внутри external-agents.
2. Реальный Cursor worker вместо stub `/v1/cursor/exec`.
3. Перенос Секретаря в core-контейнер с DNS на `external-agents`.
4. По желанию: перевести сервисы основного compose на сеть `quazar` отдельным окном обслуживания.
