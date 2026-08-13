# Миграция с Hermes (2026-08-14)

## Сделано
- Секреты Секретаря/OpenRouter → `/var/lib/quazar/secrets/`
- YouTube scripts + Google token → `/var/lib/quazar/youtube/`
- Python venv → `/var/lib/quazar/venv-youtube/`
- Wrappers `/usr/local/bin/quazar-youtube-*` больше не делают `docker exec hermes-gateway`
- `quazar-chat-send` читает ключ из `/var/lib/quazar/secrets/`
- `hermes` в `docker-compose.yml` — только profile `hermes` (по умолчанию не стартует)
- Модели чата → `/var/lib/quazar/config/chat-models.yaml`

## Не удалено
- `/root/.hermes` оставлен как бэкап (секреты, SOUL, старые скрипты)
- API ядра `GET/POST /hermes/event*` — журнал внешних событий (имя историческое)

## Включить Hermes снова (отладка)
```bash
docker compose --profile hermes up -d hermes
```
