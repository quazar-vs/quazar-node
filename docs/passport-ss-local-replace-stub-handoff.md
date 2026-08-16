# HANDOFF: Passport — заменить stub SOCKS на реальный ss-local

Скопируй локальному агенту Cursor на ПК  
(репо: `/home/ser/projects/quazar_passport/`).  
**На VPS Flutter не править и APK не собирать.**

Канон постановки (сервисы / lifecycle / UX):  
→ [`passport-citizen-online-service-vpn-handoff.md`](./passport-citizen-online-service-vpn-handoff.md)

Бэкенд на VPS **уже живой**: `/api/vpn/ss/*` + `ssserver` (порт **8388**, ACL YouTube/Telegram).  
Этот документ — только **клиентский** шаг: снять stub и подключить настоящий Shadowsocks local.

---

## Status

| Сторона | Состояние | Дата |
|---------|-----------|------|
| Узел `/opt/quazar` | **Done** — API + ssserver | ~2026-08-16 |
| Клиент Passport | **Done** — client ss-local (`libsslocal.so`, NativeSsLocal, QuazarVpnService TUN→SOCKS, stub removed) | ~2026-08-16 |
| Далее | **Next** — user QA on device | ~2026-08-16 |

---

## 1. Цель / поведение

Когда гражданин **online** и включён toggle сервиса (**YouTube** / **Telegram**):

1. Трафик пакетов сервиса идёт через тонкий `VpnService` → локальный SOCKS.
2. Локальный SOCKS — **реальный ss-local** (не stub): шифрованный канал к `ssserver` на узле.
3. Остальной телефон — **direct** (как сейчас со split VpnService).

Когда **offline** / выход:

- Stop TUN + stop ss-local (ничего не слушает).
- Revoke SS-сессии на сервере.
- `POST /offline`.

**Не трогать:** SSH (уже убран), WireGuard (не блокер), полный device-wide VPN.

---

## 2. Контракт API (из `src/vpn.rs` / `src/main.rs` — не выдумывать)

Auth: Citizen+ API key — `Authorization: Bearer <key>` или `X-Api-Key: <key>`.  
Обёртка ответа: `{ "status": "success"|"error", "data": …, "error": … }`.

Базовый публичный API (ориентир): `https://api.quazar.website`  
(внутренний `QUAZAR_NODE_URL` на VPS может быть `http://<ip>:8080` — **не** путать с SS host).

### `POST /api/vpn/ss/credential`

Request (все поля optional; пустое тело ок):

```json
{
  "citizen_id": "<optional; default = auth citizen>",
  "ttl_hours": 24,
  "services": ["youtube", "telegram"]
}
```

- `ttl_hours`: 1…168; default из `SS_TTL_HOURS` (обычно 24).
- `services`: **принимается, но на сервере сейчас игнорируется** (`let _services = req.services`). ACL профиля фиксирован.

Response `data` (`SsCredentialResponse`):

| Поле | Тип | Смысл |
|------|-----|--------|
| `session_id` | string (uuid) | = `vpn_session_id` для `activate_vpn` |
| `server` | string | публичный host узла для ss-local |
| `host` | string | **то же значение**, что `server` (дубликат для удобства клиента) |
| `port` | number | обычно `8388` (`SS_PORT`) |
| `method` | string | обычно `chacha20-ietf-poly1305` (`SS_METHOD`) |
| `password` | string | ephemeral shared password активных сессий |
| `acl_profile` | string | всегда `"citizen_youtube_telegram"` |
| `suggested_acl` | null / object | сейчас всегда `null` (не сериализуется если null) |
| `ttl_sec` | number | `ttl_hours * 3600` |
| `transport` | string | `"shadowsocks"` |
| `expires_at` | ISO datetime | срок сессии |
| `citizen_id` | string | гражданин |

Пример формы (значения host/method/port — живые дефолты прод):

```json
{
  "status": "success",
  "data": {
    "session_id": "<uuid>",
    "server": "quazar.website",
    "host": "quazar.website",
    "port": 8388,
    "method": "chacha20-ietf-poly1305",
    "password": "<ephemeral>",
    "acl_profile": "citizen_youtube_telegram",
    "ttl_sec": 86400,
    "transport": "shadowsocks",
    "expires_at": "…",
    "citizen_id": "…"
  }
}
```

Повторный issue для того же гражданина **деактивирует** предыдущую активную сессию и выдаёт новую.

### `POST /api/vpn/ss/revoke`

Request:

```json
{
  "session_id": "<optional>",
  "citizen_id": "<optional; default = auth citizen>"
}
```

- С `session_id` — revoke этой сессии (с проверкой citizen, если оба заданы).
- Без `session_id` — revoke **всех** активных SS-сессий гражданина.
- Идемпотентно.

Response `data`:

```json
{ "ok": true, "revoked": true, "transport": "shadowsocks" }
```

**Важно ops:** при успешном revoke сервер **ротирует shared password** оставшихся активных сессий и переписывает `ssserver` config. Клиенты с старым password отвалятся — им нужен новый `POST …/credential`.

### `GET /api/vpn/ss/status`

Без тела. Response `data`:

| Поле | Тип |
|------|-----|
| `is_active` | bool |
| `session_id` | string \| null |
| `expires_at` | datetime \| null |
| `last_used` | datetime \| null |
| `seconds_remaining` | number |
| `transport` | `"shadowsocks"` |
| `server` | string \| null |
| `host` | string \| null (= `server`) |
| `port` | number \| null |
| `method` | string \| null |
| `acl_profile` | string \| null |

Пароль в status **не** отдаётся.

### Связанные endpoints (уже в клиенте)

| Метод | Путь | Назначение |
|-------|------|------------|
| `POST` | `/online` | presence online |
| `POST` | `/offline` | presence offline + **сервер сам** `revoke_ss_for_citizen` (`ss_credentials_revoked` в ответе) |
| `POST` | `/api/citizen/activate_vpn` | body: `{ "vpn_session_id": "<session_id из credential>" }` |
| `POST` | `/api/vpn/revoke` | alias на SS revoke |
| `GET` | `/api/vpn/status` | SS status (`transport: "shadowsocks"`) |
| `POST` | `/api/vpn/key` | **410 Gone** — не вызывать |

---

## 3. Что заменить в Passport: stub → ss-local

### Текущее состояние (ожидаемое)

- `goOnline` → credential (API **или** stub) → `activate_vpn` → **stub SOCKS** + split `QuazarVpnService` / `addAllowedApplication`.
- Stub слушает локальный SOCKS, но **не** проксирует на узел → YouTube/Telegram через узел не работают.

### Целевое

Сохранить:

- split `VpnService` → local SOCKS;
- каталог сервисов / toggles;
- lifecycle online/offline / revoke / `activate_vpn`.

Заменить:

- реализацию `ShadowsocksTransport` / stub SOCKS → **настоящий ss-local**, который dial'ит `server:port` с `method` + `password` из credential.

### Практичный вариант для Flutter/Android (рекомендация)

**Primary recommendation:** нативный **shadowsocks-rust `sslocal`** (тот же стек, что `ssserver` на узле) через **JNI / FFI / MethodChannel**, локальный SOCKS (и UDP associate) на `127.0.0.1:<local_port>`.

Почему:

- AEAD `chacha20-ietf-poly1305` из коробки;
- TCP+UDP (сервер `mode: tcp_and_udp`);
- не тянуть Outline UI / лишний продукт;
- VpnService уже умеет гнать apps в SOCKS — меняется только бэкенд SOCKS.

Альтернативы (если sslocal JNI дорог по сроку):

| Вариант | Заметка |
|---------|---------|
| Prebuilt `libsslocal.so` / Android NDK build shadowsocks-rust | То же, без полного исходника в app |
| Библиотека уровня shadowsocks-android / Outline core (только crypto+local) | Ок, если лицензия/размер приемлемы; **не** встраивать весь Outline Client |
| Pure Dart SS | Не рекомендуется для prod (скорость / UDP / AEAD) |

**Не делать:** SSH tunnel, системный Wi‑Fi proxy как основной путь, полный VPN без allowlist.

Клиентский ACL optional v1: серверный ACL уже режет non-YouTube/Telegram; клиентский bypass-ACL — nice-to-have, не блокер E2E.

---

## 4. Lifecycle

### Online / Connect

1. `POST /online` `{ "citizen_id" }`
2. `POST /api/vpn/ss/credential` → сохранить `session_id`, `server`/`host`, `port`, `method`, `password`
3. Если ещё не active: `POST /api/citizen/activate_vpn` `{ "vpn_session_id": "<session_id>" }`
4. **Start ss-local** с этими параметрами → local SOCKS
5. Если есть включённые сервисы с установленными пакетами → start split `VpnService` → SOCKS
6. FGS только пока online

### Toggle сервиса (уже online)

- Пересобрать allowlist packages / soft-restart VpnService.
- ss-local **не** обязан рестартовать, если endpoint не менялся.

### Offline / Disconnect

1. Stop VpnService / TUN  
2. **Stop ss-local** (порт свободен)  
3. `POST /api/vpn/ss/revoke` `{ "session_id" }` и/или `{ "citizen_id" }`  
4. `POST /offline` `{ "citizen_id" }` — сервер тоже revoke'ит; клиентский revoke всё равно желателен (явный порядок, меньше гонок)  
5. Стереть password из памяти/secure storage сессии

### Сеть Wi‑Fi↔LTE

- Если ещё online: reconnect ss-local с **теми же** credentials (пока не expire / не revoke).
- После revoke/offline — ничего не поднимать.

### Ротация password

Если revoke другого клиента (или свой) ротирует shared password — текущий ss-local умрёт. Поведение: при ошибках dial / auth → `POST …/credential` заново (если ещё online) и restart ss-local.

---

## 5. Host / endpoint узла

| Источник | Значение для клиента |
|----------|----------------------|
| Поля credential | **`server` и `host`** (одинаковые) — **использовать их**, не хардкодить IP |
| Env на узле | `SS_PUBLIC_HOST` (fallback `QUAZAR_PUBLIC_HOST`, иначе `quazar.website`) |
| Прод сейчас | `SS_PUBLIC_HOST=quazar.website`, `SS_PORT=8388`, `SS_METHOD=chacha20-ietf-poly1305` |
| `QUAZAR_NODE_URL` | URL **HTTP API** узла (peers/docker), **не** SS host. Пример на VPS: `http://176.98.191.38:8080` |

Клиент: `ss-local` → `host`/`server` + `port` из credential (TCP+UDP на 8388).  
API calls: базовый URL приложения (`https://api.quazar.website` или как уже настроено в Passport).

Серверный ACL (outbound allowlist): YouTube/Telegram domains + Telegram DC CIDR — см. `deploy/ss/citizen.acl`. Иной трафик через ssserver **не** пройдёт.

---

## 6. Acceptance criteria

- [ ] Stub SOCKS удалён / не используется в prod-path `ShadowsocksTransport`.
- [ ] Online + toggle YouTube/Telegram → реальный ss-local к `server`/`host`:`port` с `method`/`password` из API.
- [ ] YouTube app и/или Telegram реально ходят через узел (не direct), пока online+toggle.
- [ ] Offline → ss-local stopped, локальный SOCKS порт не слушает, VpnService down.
- [ ] Revoke вызывается при offline; после revoke старый password не работает.
- [ ] `activate_vpn` получает `session_id` из credential (не SSH).
- [ ] SSH по-прежнему отсутствует.
- [ ] Нет хардкода секретов; host берётся из ответа API (ожидаемо `quazar.website`).

---

## 7. Как проверить вручную

### A. API (с Citizen API key, без печати password в логи/тикеты)

```bash
# credential
curl -sS -X POST "https://api.quazar.website/api/vpn/ss/credential" \
  -H "Authorization: Bearer $CITIZEN_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{}'

# status
curl -sS "https://api.quazar.website/api/vpn/ss/status" \
  -H "Authorization: Bearer $CITIZEN_API_KEY"

# activate (session_id из credential)
curl -sS -X POST "https://api.quazar.website/api/citizen/activate_vpn" \
  -H "Authorization: Bearer $CITIZEN_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"vpn_session_id":"<session_id>"}'

# revoke
curl -sS -X POST "https://api.quazar.website/api/vpn/ss/revoke" \
  -H "Authorization: Bearer $CITIZEN_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"session_id":"<session_id>"}'
```

Ожидание credential: `server`/`host` ≈ `quazar.website`, `port` `8388`, `method` `chacha20-ietf-poly1305`, `transport` `shadowsocks`.

### B. Desktop ss-local smoke (до/параллельно с Flutter)

На ПК с shadowsocks-rust:

```bash
sslocal -b "127.0.0.1:1080" \
  -s "<host>:<port>" \
  -m "<method>" \
  -k "<password>"
```

Проверка: `curl --socks5-hostname 127.0.0.1:1080 https://www.youtube.com -I` — успех; запрос к произвольному non-ACL хосту — отказ/блок на стороне сервера.

### C. На устройстве

1. Online + YouTube ON → открыть YouTube app → трафик через VPN notification / нет «фейкового» stub.
2. Offline → VPN notification исчез, SOCKS не слушает, YouTube снова direct.
3. После revoke повторный online получает новый password и снова работает.

---

## 8. Промпт для локального агента

```
По docs/passport-ss-local-replace-stub-handoff.md (канон lifecycle —
docs/passport-citizen-online-service-vpn-handoff.md) в репо
/home/ser/projects/quazar_passport:

Снять stub SOCKS в ShadowsocksTransport и подключить реальный ss-local
к живому бэкенду Quazar (POST /api/vpn/ss/credential и т.д.).

- Использовать точные JSON-поля из handoff (server+host, port, method,
  password, session_id → activate_vpn).
- Практично: shadowsocks-rust sslocal через JNI/FFI/MethodChannel;
  сохранить split VpnService → local SOCKS для YouTube/Telegram.
- Lifecycle: start/stop ss-local с online/offline; revoke; не SSH.
- Host из ответа API (не хардкод IP); method chacha20-ietf-poly1305.
- Acceptance и ручные проверки — из handoff §6–7.

Flutter только на локальном ПК. На VPS app не трогать.
```
