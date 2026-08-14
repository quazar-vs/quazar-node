# HANDOFF: Passport — стабильный SSH-«VPN» (без обрывов при видео)

Скопируй локальному агенту Cursor на ПК  
(репо: `/home/ser/projects/quazar_passport/`).  
**На VPS Flutter не править и APK не собирать.**

---

## Проблема

«ВПН» в Passport — это **SSH local-forward** (`quazar-tunnel`), не WireGuard.  
Трафик роликов через туннель обычно не идёт, но пока пользователь смотрит видео, Android/iOS уводит процесс в фон → SSH перестаёт отвечать на keepalive → **sshd убивает сессию** (`Timeout, client not responding`). Приходится вручную перезапускать ВПН.

На сервере уже сделано (VPS, 2026-08-14):
- sshd `ClientAlive` стал терпеливее (~15 мин тишины): `ClientAliveInterval 60`, `CountMax 15`, `TCPKeepAlive yes`
- API hint для клиента: `keepalive_interval_secs=20`, `keepalive_count_max=9` (ServerAlive)

**Без правок приложения обрывы всё равно будут** — нужен живой SSH в фоне + автореконнект.

---

## Цель

Пока ВПН «включён» в UI:
1. SSH-туннель **живёт** при просмотре видео / свёрнутом приложении (foreground service / аналог).
2. Используются keepalive из ответа API (`tunnel.keepalive_interval_secs` / `keepalive_count_max`) как `ServerAliveInterval` / `ServerAliveCountMax`.
3. При обрыве — **автореконнект** с backoff без ручного выкл/вкл; пользователь видит статус «переподключение…».
4. После успешного реконнекта API через туннель снова работает; роль/`activate_vpn` не ломается (идемпотентно).

---

## Контракт ядра (уже)

1. `POST /api/vpn/key` → `id`, ключи, объект **`tunnel`**:
   - `ssh_host`, `ssh_port`, `ssh_user` (или top-level `ssh_user`)
   - `client_local_port` (часто `18443`) → forward на `remote_api_host:remote_api_port`
   - `keepalive_interval_secs`, `keepalive_count_max`
   - `api_base_url_via_tunnel` (например `http://127.0.0.1:18443`)
   - `ssh_local_forward_command` — подсказка; реализовать эквивалент в нативном SSH-клиенте
2. Сразу: `POST /api/citizen/activate_vpn` `{ "vpn_session_id": "<id>" }`
3. Дальше API ходить на `api_base_url_via_tunnel`, пока туннель up.
4. Heartbeat (если уже есть endpoint туннеля) — слать периодически; не заменяет SSH ServerAlive.

Связанный канон роли: `docs/passport-vpn-role-handoff.md` (не дублировать, не ломать).

---

## Что сделать в приложении

### A. Keepalive на клиенте (обязательно)
- При старте SSH выставлять:
  - `ServerAliveInterval` = `tunnel.keepalive_interval_secs` (fallback **20**)
  - `ServerAliveCountMax` = `tunnel.keepalive_count_max` (fallback **9**)
  - `TCPKeepAlive=yes` если библиотека умеет
- Не глушить keepalive в фоне.

### B. Android: Foreground Service
- Пока VPN toggle ON — **foreground service** с постоянным notification («Квазар: защищённое соединение» / аналог).
- SSH-сессия живёт в этом сервисе, не в UI isolate, который убивают при видео.
- `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` — по желанию/политике магазина; минимум — FGS.
- Обработать смену сети (Wi‑Fi↔LTE): reconnect.

### C. iOS (если есть)
- Не полагаться на background fetch для SSH.
- Пока VPN «включён»: держать соединение максимально допустимым способом (NWConnection / tunnel provider, если уже есть); иначе честно показать, что iOS рвёт фон — и автореконнект при возврате в foreground.
- Если используете Network Extension / Packet Tunnel — keepalive там.

### D. Автореконнект
- Детект: socket closed / timeout / probe на `api_base_url_via_tunnel` (лёгкий GET health/status).
- Политика: сразу 1 ретрай, затем backoff 2s → 5s → 15s → 30s (кап ~60s), пока toggle ON.
- Не выпускать новый `vpn/key` на каждый чих — сначала переподнять SSH тем же ключом; новый key только если auth rejected / ключ отозван.
- UI: `connected` | `reconnecting` | `failed` (с кнопкой «Подключить снова»).

### E. Не делать
- Не строить полный WireGuard в этом handoff.
- Не править VPS / Flutter на VPS.
- Не требовать ручного «перезапусти VPN» как нормальный UX.

---

## Критерии приёмки

- [ ] Включённый ВПН + 10+ минут просмотра видео (YouTube и т.п.) в другом приложении → туннель жив **или** тихо реконнектится без действия пользователя
- [ ] В логах/UI видно использование ServerAlive из API (20/9 или актуальные поля `tunnel`)
- [ ] Обрыв сети → статус reconnecting → снова connected; биржа/API не требуют ручного VPN restart
- [ ] Повторный `activate_vpn` безопасен; лишние `vpn/key` не спамятся
- [ ] Выключение VPN в UI гасит FGS и SSH

---

## Подсказка по симптомам на сервере

Если после релиза всё ещё есть:
`Timeout, client not responding from user quazar-tunnel`  
→ клиент всё ещё не шлёт ServerAlive или процесс убит. Сверить FGS и опции SSH.

Промпт для агента:
```
По docs/passport-vpn-keepalive-handoff.md (в репо quazar-node / скопируй текст handoff) исправь Passport SSH-VPN: foreground service, ServerAlive из tunnel.*, автореконнект. На VPS Flutter не трогать. Критерии приёмки из handoff.
```
