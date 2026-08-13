# Сверка рефлексов (2026-08-14)

Эталон: `docs/algonism/REFLEXES.md`. Живой голос: `/usr/local/bin/quazar-chat-direct.py` (копия в репо: `scripts/quazar-chat-direct.py`).

| Рефлекс | Ядро | Секретарь (до) | Секретарь (после) |
|---------|------|----------------|-------------------|
| `RX_GUEST_HELLO` | — | intro | без изменений |
| `RX_CITIZEN_APPLY_GUIDE` / passport | API | guest/candidate focus | без изменений |
| `RX_STATUS_NAVIGATE` | VPN/роль | tier + focus_map | citizen focus → заявка |
| `RX_SVOD_EXPLAIN` | `GET /svod` → `lots` | «Разработка/Реализация» | **один список `lots`** |
| `RX_EXCHANGE_GLOBAL_*` | Agent → только global | не сказано | **в system + need_gate** |
| `RX_NEED_ACCEPT` | `POST /needs` | после need → `/buy` | **заявка; buy не главный шаг** |
| `RX_DEAL_AUTO_CONFIRM` | `meets_access_level` | «каждая сделка ждёт СБ» | **авто / эскалация** |
| `RX_LOT_LIST` / export / shelf | exchange v3 | слабо | кратко в промпте |
| `RX_SHOP_*` | нет контура | нет | отложено |

## Проводка
- Правки в `scripts/quazar-chat-direct.py` → установлено в `/usr/local/bin/`, `systemctl restart quazar-chat-direct` → **active**.
- Awaken OK (`source=github`, laws incl. `svod`).
- Smoke: нет «каждая сделка ждёт СБ»; есть global-only и «один список лотов».

## Осталось вне Секретаря
- Живой контур магазина (`RX_SHOP_*`) — когда Мэр развернёт витрину.
- LLM upstream 401 на части моделей — отдельно от канона рефлексов.
