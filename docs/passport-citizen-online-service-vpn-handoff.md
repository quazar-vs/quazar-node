# HANDOFF: Passport — эфемерный VPN к сервисам (гражданин online)

Скопируй локальному агенту Cursor на ПК  
(репо: `/home/ser/projects/quazar_passport/`).  
**На VPS Flutter не править и APK не собирать.**  
Бэкенд/контракт узла: репо `/opt/quazar` (этот файл — канон постановки).

Бывшее имя: `passport-citizen-online-per-app-vpn-handoff.md` (тот же документ, модель уточнена: **сервисы**, не произвольный список приложений).

Связанные handoff (не ломать, не дублировать целиком):
- `docs/passport-vpn-role-handoff.md` — **ВПН = принятие роли** (`activate_vpn`, online → active)
- `docs/passport-vpn-keepalive-handoff.md` — **устаревает для клиента**: описывал SSH local-forward; для Passport **не** продолжать. Новые сессии — только VPN/SS (этот документ).

---

## Status / прогресс

| Сторона | Состояние | Дата |
|---------|-----------|------|
| **Клиент Passport (Flutter, ПК)** | **Done** — client ss-local (`libsslocal.so`, NativeSsLocal, QuazarVpnService TUN→SOCKS, stub removed) | ~2026-08-16 |
| **Узел `/opt/quazar` (SS API + ssserver)** | **Done** — `/api/vpn/ss/*`, ssserver+ACL, revoke на `/offline`; **SSH tunnel в ядре удалён** | ~2026-08-16 |
| **Далее** | **Next** — user QA on device | ~2026-08-16 |

### Клиент (готово — ss-local)

- **SSH убран:** VPN start без SSH extras; SSH MethodChannel → `SSH_REMOVED`; `dartssh2` удалён; `SshTunnel` — отказ.
- **Lifecycle:** `goOnline` → `POST /online` → SS credentials → `activate_vpn` → **реальный ss-local** + split VpnService; `goOffline` → stop TUN/SOCKS → revoke → `POST /offline`.
- **UX:** «Сеть и сервисы» — online + toggles YouTube / Telegram.
- **Primary:** `libsslocal.so` + NativeSsLocal; `QuazarVpnService` TUN→SOCKS; stub SOCKS **снят**.
- **API-контракт клиента:** `requestSsCredential`, `revokeSsCredential`, `activateVpn`.
- TUN только если включён сервис с установленным пакетом.
- Тест: `test/vpn_service_catalog_test.dart` ok.

### Следующий шаг

**User QA on device** — online + YouTube → через узел; offline → off.  
Детали клиента: [`passport-ss-local-replace-stub-handoff.md`](./passport-ss-local-replace-stub-handoff.md).  
SSH tunnel API в ядре **удалён** (`POST /api/vpn/key` → 410 Gone).
---

## 0. Канон (зафиксировано)

> **Мы удаляем все каналы SSH для приложения, просто поднимаем VPN между приложением и сервером.**

| Правило | Смысл |
|---------|--------|
| **SSH out** | В Passport **полностью убрать** SSH-туннели, `ssh-key` VPN channels, local-forward, любой SSH как транспорт. **Не** interim, **не** fallback. |
| **Только VPN app↔server** | Между приложением и сервером — **только VPN** (см. §4). Online гражданина → VPN **up**; offline → VPN **down**. |
| **Сервисы** | UX: YouTube / Telegram (каталог сервисов), не произвольный app-picker. |
| **Primary recommendation** | **Shadowsocks-rust + ACL** + тонкий **VpnService/tun2socks** на клиенте (завернуть только эти сервисы; остальное direct). |
| **Альтернатива** | **WireGuard** split / AllowedApps — полноценный «просто VPN» end-to-end; **без SSH**. Не primary для этой цели. |

---

## 1. Цель / поведение

Согласованная модель (не «три режима», не полный туннель 24/7):

| Событие | Поведение телефона |
|--------|---------------------|
| Гражданин **вошёл** / статус «в сети» (presence online + сессия гражданина) | Поднять **VPN-сессию** (primary: **ss-local + ACL** + тонкий слой доставки к YouTube / Telegram). Остальной телефон — **direct**. |
| Гражданин **вышел** / offline | **Сразу stop:** VPN (ss-local + VpnService/tun helper, или WG peer). Ничего не слушает; чужой трафик не трогается. |

Это **эфемерный VPN к выбранным сервисам через узел**, а не постоянный туннель на всём устройстве и не «выбери любое приложение из списка».

### Продуктовая модель: сервисы, не «любые apps»

UX — переключатели **известных сервисов** (иконка + имя):

| Сервис (UI) | Online + toggle ON | Offline / toggle OFF |
|-------------|--------------------|----------------------|
| **YouTube** | Трафик сервиса через узел (SS / WG split) | Не трогать |
| **Telegram** | Трафик сервиса через узел (SS / WG split) | Не трогать |
| … (расширяемый каталог) | то же | то же |

Пользователь **не** видит произвольный picker установленных пакетов. Он включает «YouTube», «Telegram» — продукты, которыми пользуется.

**Явно out of scope этого sprint:**
- Три пользовательских режима («только Квазар» / «ручной прокси» / «полный туннель») — **не раздувать**.
- Ручной Wi‑Fi proxy как основной путь.
- Полный device-wide VPN (весь трафик телефона) без split/ACL и «маршрутизация всей системы».
- UX «список всех установленных приложений с чекбоксами» как основной сценарий.
- Любые **SSH**-каналы в приложении (туннель, key channel, fallback).
- Ломать прод-узел «с нуля» без миграции credentials (см. §6: deprecate SSH API → новый VPN/SS API).

Фокус одной фразой: **online гражданин = VPN up (YouTube/Telegram через узел); offline = VPN down. SSH в app — нет.**

---

## 2. Техническая правда Android (UI ≠ API)

### 2.1 Сервисы → packages / домены

Android `VpnService` / `Builder.addAllowedApplication` работает по **package UID**, не по бренду «YouTube».

Поэтому handoff явно разделяет слои:

| Слой | Что видит / что делает |
|------|-------------------------|
| **UI** | «YouTube», «Telegram» — иконки и названия **сервисов** |
| **Под капотом** | Map **service id → известные package names** и/или **ACL по доменам/IP** сервиса (предпочтительно для SS) |

### Пример map (ориентир, уточнять на устройстве)

| Service id | Типичные package names |
|------------|------------------------|
| `youtube` | `com.google.android.youtube`, `com.google.android.youtube.tv`, … |
| `telegram` | `org.telegram.messenger`, `org.telegram.messenger.web`, Telegram X (`org.thunderdog.challegram`), … |

Хранилище: toggles по **service id** (`youtube: true`), не сырой список package names как главный UX-state (packages — derived).

### Браузер ≠ приложение YouTube

**Не обещать** «весь YouTube» (включая просмотр в Chrome/Firefox) только от toggle «YouTube».

- Toggle **YouTube** = пакеты приложения YouTube (и TV/варианты из map) **и/или** ACL-домены YouTube, если трафик идёт через ss-local с proxy-only ACL.
- Трафик `youtube.com` / `googlevideo.com` **из браузера** — другой UID. Без отдельной логики он **не** попадёт в package-allowlist; с **domain ACL** на ss-local — может, если браузер направлен в SOCKS (обычно нет без VpnService).

Опции (выбрать явно в UX, не смешивать молча):

1. **v1:** apps из map через тонкий VpnService→SOCKS **или** только то, что реально проксируется ACL; в UI подпись вроде «приложение YouTube» / без обещания браузера.
2. **Опционально:** отдельный сервис/флаг «YouTube в браузере» — через домены ACL + доставка браузера в SOCKS (осторожно).
3. Не делать «весь браузер через VPN» под видом YouTube.

То же правило для других веб-сервисов, если появятся.

### 2.2 Честно: ss-local ≠ «приложения сами пойдут»

**Shadowsocks-rust `ss-local` даёт локальный SOCKS** (и опционально HTTP). Сам по себе он **не** заставляет YouTube/Telegram ходить в прокси без дополнительной доставки:

| Путь | Суть | Заметка |
|------|------|---------|
| **A. Тонкий VpnService + tun2socks** (предпочтительно для apps без SOCKS) | Пока citizen online: `VpnService` только для packages включённых сервисов → локальный SOCKS ss-local. **Не** полный системный VPN на всё. | Один системный диалог VPN; FGS только online. |
| **B. Нативный SOCKS в клиенте** | Telegram умеет SOCKS; **YouTube app обычно нет**. | Можно для Telegram; YouTube всё равно нужен путь A (или браузер с прокси — out of default). |
| **C. Ручные настройки в каждом app** | Пользователь сам вбивает прокси | **Out of scope** как основной UX. |

Итого: транспорт к узлу — **SS + ACL** (быстро и без маршрутизации всей ОС). На Android для **приложений** без встроенного SOCKS всё ещё нужен **тонкий** слой A на время online — это и есть «VPN между приложением и сервером» в primary-стеке, не отказ от SS и не «полный VPN на весь телефон».

---

## 3. Контекст: что уже есть на узле (legacy SSH — deprecate)

Сейчас «ВПН» в Passport на проде ещё завязан на **SSH local-forward** (`quazar-tunnel`) + акт принятия роли. Это **исторический** путь. **Канон клиента:** SSH **полностью убрать**; при пилении бэкенда — deprecate/remove SSH credential flow для app, заменить VPN/SS API (см. §6).

### Presence / lifecycle

| API | Назначение |
|-----|------------|
| `POST /online` `{ "citizen_id" }` | Presence `citizen_status=online`; lifecycle `citizens.status=online` только если паспорт выдан и был `pending`/`online` (**не** demote `active`). |
| `POST /offline` `{ "citizen_id" }` | Presence `offline`. **Сейчас не** отзывает VPN/SS credentials и **не** меняет `citizens.status` (active остаётся active). Цель: при offline — revoke VPN/SS session на сервере. |
| `GET /citizens/online` | Список online за ~5 мин (не для клиента VpnService). |

Ответ `/online` намекает: *"Enable VPN to become active."* — трактовать как **VPN/SS**, не SSH.

### Роль (канон роли сохранить)

| API | Назначение |
|-----|------------|
| `POST /api/citizen/activate_vpn` `{ "vpn_session_id" }` | **Единый акт:** валидная VPN-сессия → `citizens.status=active` + роль (Guardian+/Appointed). Идемпотентно. `vpn_session_id` = id **VPN/SS** (или WG) credential, **не** SSH key. |
| `GET /api/vpn/keys` | Admin (Aiya/Founder) — пересмотреть под новый credential store. |

Пока `citizens.status=online` (ещё не active), биржа закрыта — нужен `activate_vpn` (см. `http.rs` `allows_online_citizen`).

### Legacy SSH APIs (removed from kernel ~2026-08-16)

| API | Статус |
|-----|--------|
| `POST /api/vpn/key` | **Removed** — `410 Gone` (текст: используйте `/api/vpn/ss/credential`). |
| `GET /api/vpn/status` | **SS status** (`transport: "shadowsocks"`). |
| `POST /api/vpn/revoke` | **SS revoke** (то же, что `/api/vpn/ss/revoke`). |
| `GET/POST /api/tunnel/*` | **Removed** из `quazar-node` (продуктовый SSH tunnel). |

Host **sshd для админов** не трогаем. Env `SSH_TUNNEL_*` / `VPN_AUTHORIZED_KEYS_*` ядро **игнорирует**.

---

## 4. Транспорт: целевой стек (согласовано, без SSH)

### Согласованная позиция

Между приложением и сервером — **только VPN**. Цель — **завернуть трафик к YouTube и Telegram** (не весь телефон):

1. **Primary (рекомендация):** **Shadowsocks-rust + ACL** + тонкий **VpnService/tun2socks** на клиенте — быстрый путь «завернуть» сервисы; остальное **direct**. Без SSH.
2. **Альтернатива («просто VPN» end-to-end):** **WireGuard** split / AllowedApps — полноценный UDP-туннель app↔server; **без SSH**. Не default для этой цели (тяжелее операционно / другой ops-контур), но допустим как второй транспорт в абстракции.
3. **Продукт:** online → VPN **up** (сервисы через узел); offline → VPN **down**.
4. **SSH** — **запрещён** в приложении: ни туннель, ни key channel, ни fallback.

### Primary vs альтернатива

| Вариант | Роль | Суть |
|---------|------|------|
| **SS + ACL + лёгкий TUN** | **Primary** | ss-local + ACL (proxy YouTube/Telegram); VpnService→SOCKS только для packages сервисов. |
| **WireGuard split** | Альтернатива | WG peer; AllowedApps / routes только под сервисы. Тот же UX online/offline. |
| **SSH** | **Удалён** | Не реализовывать, не оставлять «на всякий случай». |

### Целевой стек (primary)

| Сторона | Компонент |
|---------|-----------|
| **Сервер (узел Quazar)** | `shadowsocks-rust` **ssserver**; ACL и/или **отдельный порт/профиль для граждан**; credentials на гражданина. |
| **Клиент Passport** | **ss-local** + **ACL** (proxy-only YouTube/Telegram domains/IPs, else bypass/direct); тонкий VpnService→SOCKS при необходимости. |
| **Альтернатива** | WireGuard + AllowedApps / split (тот же lifecycle online/offline). |
| **Запрещено в app** | SSH local-forward, ssh-key channels, любой SSH-транспорт. |

### ACL (ориентир, primary)

- Режим в духе **proxy только listed** (bypass остальное / `[[bypass_all]]` + proxy rules — уточнить синтаксис shadowsocks-rust ACL при внедрении).
- Списки: домены/IP/CIDR YouTube, Telegram (и CDN, необходимые для работы).
- Клиентский ACL должен совпадать по смыслу с серверным ограничением (сервер не обязан принимать «весь интернет» для граждан — отдельный порт/ACL граждан).

### Миграция (клиент)

1. Абстракция `CitizenVpnSession`: `start(online)` / `stop(offline)` — **только** VPN-транспорты.
2. Реализации: **`ShadowsocksTransport` (primary)** → опционально `WireGuardTransport`. **Нет** `SshTransport`.
3. Удалить из app: SSH connect, key storage для туннеля, local-forward, зависимости на `/api/vpn/key` SSH shape.
4. `activate_vpn` + роль **не привязывать** к типу транспорта: нужен валидный `vpn_session_id` с узла (SS или WG credential id).
5. Квазар-API: можно идти **прямым HTTPS** к публичному API даже при offline; VPN/SS — для включённых сервисов.

### Не делать в этом handoff

- Оставлять SSH «на переходный период» в приложении.
- Полный rewrite узла «сразу» без отдельного backend PR.
- Требовать WireGuard на проде как блокер primary (SS).
- **Реализовывать ssserver на узле в рамках этого документа** — отдельный backend PR (см. §6); здесь только постановка.
- Полный системный VPN без ACL / без ограничения сервисами.

---

## 5. Что сделать в Passport (Android-first) — **клиент done ~2026-08-16**

Ниже — канон поведения (уже реализовано на ПК). Дальнейшие правки Flutter — только на локальном репо; на VPS не трогать.

### A. Жизненный цикл = гражданин в сети (= VPN up/down)

1. **Войти / Connect / «в сети»:**
   - `POST /online`
   - Запросить/обновить **VPN credentials** (**цель:** SS endpoint + method/password/ACL; альтернатива — WG peer config)
   - Если ещё не active: `POST /api/citizen/activate_vpn` (канон роли — одним UX-актом с «входом в сеть»)
   - **VPN up:** старт **ss-local + ACL**; при необходимости — тонкий **VpnService/tun2socks** только для packages включённых сервисов  
     *(или WG up, если выбран альтернативный транспорт)*
2. **Выйти / offline:**
   - **VPN down:** сначала **stop** VpnService/tun helper и **ss-local** (и FGS) — или stop WG
   - Revoke credentials (`POST /api/vpn/ss/revoke` / общий revoke)
   - `POST /offline`
   - UI: «Трафик сервисов не маршрутизируется»; **ничего не слушает** на локальных портах
3. **Жёстко:** приложение **никогда** не открывает SSH и не запрашивает SSH tunnel credentials.

### B. Каталог сервисов (UX)

- Экран: переключатели **сервисов** (YouTube, Telegram, …) с иконками — не список всех apps.
- Локальный каталог `ServiceDefinition { id, title, icon, packageNames[], aclDomainsOrCidrs[] }`.
- Хранить toggles по `service id`; при смене — пересобрать ACL / allowlist packages и soft-restart ss-local / VpnService без смены статуса гражданина.
- По умолчанию: **все OFF или узкий пресет**. Пользователь явно включает сервисы.
- Не установлен ни один пакет сервиса → честный UI («приложение не найдено»), без фейкового «включено» (если опираетесь на package path).

### C. Системный VPN permission (путь A / WG)

- Один раз: Android «Запрос VPN» (`VpnService.prepare`) — если используете тонкий VpnService/tun2socks **или** WG.
- Отказ = честный UI: «без разрешения нельзя направить YouTube/Telegram через VPN».
- Диалог VPN **не** означает «весь телефон через Квазар» — в UI формулировать как доступ к сервисам.

### D. Батарея / фон

- FGS + notification **только пока online** («Квазар: гражданин в сети» / аналог).
- Offline → снять FGS полностью; **VPN down**.
- Смена сети Wi‑Fi↔LTE: reconnect **если** ещё online; иначе ничего не поднимать.
- Не просить ignore-battery как обязательный шаг; опционально.

### E. iOS (если есть таргет)

- Network Extension / Packet Tunnel с per-app / per-service **если** entitlements позволяют; иначе честно ограничить Android-first. Транспорт — SS или WG, **не** SSH.

### F. Файлы / зоны поиска в репо Passport (ориентиры)

Точные пути на ПК уточни локально (`rg` / структура `lib/`). Типичные зоны:

- **Удалить / заменить:** VPN/SSH/tunnel service, SSH method channel, local-forward
- ss-local / outline-ish / tun2socks bindings (или WG)
- Экраны connect / online / «ВПН» / настройки сети / toggles сервисов
- Каталог service → packages + ACL domains
- API client: `/online`, `/offline`, новый `/api/vpn/ss/*` (не SSH `/api/vpn/key`), `activate_vpn`
- Хранилище сессии гражданина / статуса

Не править код на VPS.

---

## 6. Backend (узел `/opt/quazar`) — **done**

Клиентский канон **готов** (stub). Backend ниже **уже на VPS**. Следующий шаг клиента — [`passport-ss-local-replace-stub-handoff.md`](./passport-ss-local-replace-stub-handoff.md).
### 6.1 Контракт `/api/vpn/ss/*` (совместим с клиентом)

Клиент вызывает: `requestSsCredential` / `revokeSsCredential` / `activateVpn`.

| Метод | Путь | Клиент | Назначение |
|-------|------|--------|------------|
| `POST` | `/api/vpn/ss/credential` | `requestSsCredential` | Выдать эфемерные SS credentials гражданину |
| `POST` | `/api/vpn/ss/revoke` | `revokeSsCredential` | Отозвать сессию (идемпотентно) |
| `GET` | `/api/vpn/ss/status` | опционально | Статус SS-сессии гражданина (если удобнее отдельно от legacy `/api/vpn/status`) |

**`POST /api/vpn/ss/credential`** (Citizen+, после/`с` online):

Request (минимум): `{ "citizen_id": "…" }` — при необходимости `services: ["youtube","telegram"]`.

Response (цель, поля под ss-local):

```json
{
  "session_id": "<uuid — это vpn_session_id для activate_vpn>",
  "server": "<host узла>",
  "port": 8388,
  "method": "aes-256-gcm",
  "password": "<ephemeral>",
  "acl_profile": "citizen_youtube_telegram",
  "suggested_acl": null,
  "ttl_sec": 86400,
  "transport": "shadowsocks"
}
```

**`POST /api/vpn/ss/revoke`:**

Request: `{ "session_id": "…" }` и/или `{ "citizen_id": "…" }` (revoke всех SS-сессий гражданина).  
Response: `204` или `{ "ok": true }`. Идемпотентно.

**`activate_vpn`:** без смены семантики — `vpn_session_id` = `session_id` из SS credential (не SSH key id).

### 6.2 Чеклист реализации на узле

1. [x] **Маршруты** `POST /api/vpn/ss/credential`, `POST /api/vpn/ss/revoke`, `GET /api/vpn/ss/status`; store `vpn_ss_credentials`.
2. [x] **shadowsocks-rust `ssserver`** + ACL YouTube/Telegram (`/var/lib/quazar/ss/`, systemd `quazar-ssserver`).
3. [x] **`POST /offline` → revoke** SS credentials гражданина.
4. [x] **SSH tunnel в ядре удалён** (`/api/tunnel/*` removed; `/api/vpn/key` → 410). Таблица `vpn_keys` не пишется.
5. [x] `GET /api/vpn/status` → `transport: "shadowsocks"`.
6. **WireGuard** — альтернатива, не блокер primary.
7. **Опционально v1+:** `suggested_services`, per-user SS passwords (сейчас: один port + shared password среди активных сессий, rotate при revoke).
8. [x] `activate_vpn`: `vpn_session_id` = SS `session_id`.

### 6.3 Acceptance (backend)

- [x] Живой `/api/vpn/ss/credential` + `/api/vpn/ss/revoke`.
- [x] `ssserver` + ACL YouTube/Telegram.
- [x] `/offline` отзывает SS-сессии на сервере.
- [x] SSH tunnel issuance removed from kernel.

---


## 7. Acceptance criteria

### Клиент (~2026-08-16 — done, stub до backend)

- [x] **App never opens SSH** — SSH removed (`SSH_REMOVED`, нет `dartssh2` / `SshTunnel`).
- [x] **Online → VPN up;** **offline → VPN down** (lifecycle + stub SOCKS / split VpnService).
- [x] **UX сервисов:** «Сеть и сервисы» — toggles YouTube / Telegram.
- [x] **Транспорт primary:** `ShadowsocksTransport` + `QuazarVpnService` / `addAllowedApplication`; **SSH — нет**.
- [x] **Android-доставка:** split VpnService для packages сервисов; TUN только при включённом сервисе с установленным пакетом.
- [x] **Offline / выход:** stop TUN/SOCKS → revoke → `/offline`.
- [x] Канон роли: online → SS credentials (API/stub) → `activate_vpn`.
- [x] Клиентский revoke при offline (`revokeSsCredential`).
- [ ] **E2E через узел:** YouTube/Telegram реально через ssserver — **блокер: client ss-local** ([handoff](./passport-ss-local-replace-stub-handoff.md)).
- [ ] Снятие stub SOCKS → реальный ss-local.

### Backend (~2026-08-16 — done на VPS)

- [x] `/api/vpn/ss/credential` + `/api/vpn/ss/revoke` (+ revoke при `/offline`).
- [x] shadowsocks-rust `ssserver` + ACL YouTube/Telegram.
- [x] SSH tunnel в ядре удалён (`/api/vpn/key` → 410; `/api/tunnel/*` removed).
- [ ] E2E YouTube/Telegram с Passport без stub — на стороне клиента (ss-local).

---

## 8. Промпт для локального агента

**Канон клиента + backend done.** Актуальный промпт для локального агента — в  
[`passport-ss-local-replace-stub-handoff.md`](./passport-ss-local-replace-stub-handoff.md) §8  
(снять stub → реальный ss-local). Ниже — архив исходного промпта канона.

```
По docs/passport-citizen-online-service-vpn-handoff.md реализуй в Passport
эфемерный VPN к сервисам (YouTube/Telegram) при citizen online:

КАНОН: удаляем ВСЕ каналы SSH для приложения; между app и сервером —
только VPN. Online → VPN up; offline → VPN down. App never opens SSH.

- UI: toggles «YouTube», «Telegram» (иконки сервисов);
- PRIMARY: Shadowsocks (ss-local) + ACL — proxy только эти сервисы, else direct;
  Android: тонкий VpnService/tun2socks → ss-local только пока online
  (не полный системный VPN);
- АЛЬТЕРНАТИВА: WireGuard split / AllowedApps — тот же lifecycle, без SSH;
- Telegram может использовать встроенный SOCKS; YouTube app — обычно через tun helper;
- Удалить SSH tunnel / ssh-key channel / local-forward из клиента;
- сохранить канон activate_vpn / роль из passport-vpn-role-handoff.md
  (vpn_session_id = SS/WG credential, не SSH key);
- не раздувать три режима / ручной Wi‑Fi proxy / список всех apps /
  маршрутизацию всей ОС / SSH fallback.

Flutter только в /home/ser/projects/quazar_passport. Критерии приёмки из handoff.
```