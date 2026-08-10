# Портал заявок — Квазар

Форма заявки встроена в главную страницу: `/#apply` (алиас `/#заявка`).

## Канон

- **Мэрство** — только гражданину и только через приложение **Quazar Passport** (`POST /api/mayor/apply` + Bearer API-ключ).
- С лендинга/сайта заявки на мэрство **не принимаются** (`POST /api/mayor/application` → **410 Gone**).
- Путь для кандидата: **гражданство на сайте → скачать Passport → (после одобрения) мэрство в приложении**.

Без подтверждённого email форма гражданства закрыта gate’ом (вход).

UI: подача заявки на гражданство и просмотр статуса. После подачи — явный призыв скачать Passport.  
Старые ссылки `#apply/mayor…` ведут на `/#apply` с пояснением.

`applications/index.html` — редирект на `/#apply` (старые `#verify/:id` ведут на `#apply/status/:id`; также `#status/:id`, `#admin` → `#apply/admin`).

## Гражданство (public)

- `POST /api/application` — подача (тело JSON + `captcha_token`, опционально `email_already_verified`)
- `GET /api/application/:id` — статус заявки
- `GET /api/public-config` — `recaptcha_site_key` / `recaptcha_required`

Поля: `login`, `full_name`, `passport_data`, `email`, `reason`, captcha.

После submit на сайте: CTA «Скачать Passport» (URL берётся из `PASSPORT_DOWNLOAD_URL` / `GET /api/public-config`, без публичных ссылок на приватный репозиторий).

## Мэрство — citizen path only (Bearer / Passport)

Публичного apply с сайта нет. `POST /api/mayor/apply` — на protected-маршрутах, вызывается из Passport гражданином.

### Принцип «минимум внешних данных»

Снаружи заявитель передаёт **только адрес узла** (IP или DNS). Остальное ядро берёт из реестра / auth:

| Откуда | Что |
|--------|-----|
| Тело запроса | `ip` — IP **или** domain будущего узла (обязательно) |
| Тело запроса | `city` — опционально (метка; не обязательна) |
| Bearer / `X-API-Key` | API-ключ гражданина (не в JSON) |
| Реестр `citizens` | имя, email |
| Ядро | `application_id`, `sync_peer`, срок 24ч, персональный installer |

Не требуются в apply: captcha, ФИО, motivation, api-key в теле.

### API

- `POST /api/mayor/apply` — Bearer API-ключ активного гражданина  
  Тело: `{ "ip": "203.0.113.10" }` или `{ "ip": "mayor.example.com" }`  
  Опционально: `"city": "..."`  
  Ответ `201`: `{ application_id, sync_peer }`  
  После успеха: событие реестра `MayorNodeLaunchRequested` + строка в `candidates`; на email гражданина (soft-fail) уходит персональный `install-quazar-node.sh`.
- `GET /api/mayor/status/:id` — публичный статус (`status`, `node_id`, `message`)
- `GET /api/mayor/:id/package` / `GET /api/mayor/setup-package` — пакет узла (отдельный auth; вызывается из `install-node.sh` у **работающего ядра**)
- `POST /api/mayor/application` — **отключён** (410 Gone). Street-create с сайта запрещён.
- Admin `GET/POST …/api/mayor/application(s)…` — только для **legacy** street-записей (новые create не появляются).

### Алгоритм (citizen → Mayor через Passport)

1. Кандидат подаёт заявку на гражданство на сайте и скачивает Passport.
2. После одобрения входит в Passport (логин + API-ключ).
3. Гражданин в приложении вызывает `POST /api/mayor/apply` с Bearer и `{ ip }` (опц. `city`).
4. Ядро валидирует host, проверяет отсутствие активной заявки / занятого адреса.
5. Эмит **`MayorNodeLaunchRequested`** в pending/chain.
6. Локальный upsert `candidates` (kind=`mayor`, status=`pending`); peers — через projection.
7. Soft-fail: email с `install-quazar-node.sh`.
8. На сервере узла: `RECAPTCHA_*`, installer → setup-package с живого ядра.
9. Узел → **`NodeStarted`** → sync → **`MayorConfirmed`**.
10. Если за 24ч узел не стартовал → **`MayorApplicationTimedOut`**.

### События lifecycle (мэрство-запуск)

| Событие | Когда | Обязательный payload |
|---------|-------|----------------------|
| `MayorNodeLaunchRequested` | citizen `apply` | `application_id`, `citizen_id`, `node_ip`; также `citizen_name`, `sync_peer`, `expires_at`, опц. `city`, `external_email`, `source=citizen`, `status=pending` |
| `NodeStarted` | регистрация узла после setup | `application_id`, `citizen_id`, `node_id`, `url` |
| `MayorConfirmed` | узел синхронизирован | `application_id`, `citizen_id`, `node_id` |
| `MayorApplicationTimedOut` | истечение pending | `application_id`, `citizen_id`, `previous_status`, `restored_status` |

Confirm flow (`MayorConfirmed`) не менялся.

### Персональный installer

В скрипте зашиты URL ядра, API-ключ гражданина, host узла.  
Installer подключается к живому ядру за setup-package — не тащит node_secret/node_id из письма.

### reCAPTCHA (новый узел)

Ключи Google reCAPTCHA нужны на **новом узле** для его портала гражданства.  
В форму `POST /api/mayor/apply` captcha **не** входит.

## Street-заявка (сайт) — DISABLED

`POST /api/mayor/application` возвращает **410 Gone**.  
Новые street-create запрещены. Admin approve/reject legacy `confirmed` records может оставаться для добивки старых заявок.

## reCAPTCHA на портале гражданства (сайт)

Только для гражданства. Сервер проверяет токен через Google `siteverify` (`RECAPTCHA_SECRET_KEY`).

Фронт берёт site key из `window.QUAZAR_RECAPTCHA_SITE_KEY` или из `GET /api/public-config`.

## Логин в форме гражданства

Только латинские буквы `A–Z` / `a–z` (без цифр, пробелов и спецсимволов), без учёта регистра, длина **1–13**. Имя `Successful` зарезервировано.

Админ-панель гражданства: только по hash `#apply/admin` (вкладки в UI нет).
