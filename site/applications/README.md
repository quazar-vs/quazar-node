# Портал заявок — Квазар

Форма заявки встроена в главную страницу: `/#apply` (алиас `/#заявка`).

На вкладке можно выбрать тип: **гражданство** или **мэрство**. Без подтверждённого email форма закрыта gate’ом (вход).

UI: подача заявки и просмотр статуса. Шаг verify (код + «Подтвердить») убран — при `email_already_verified` бэкенд сразу ставит `confirmed`, фронт после submit открывает статус.

`applications/index.html` — редирект на `/#apply` (старые `#verify/:id` ведут на `#apply/status/:id`; также `#status/:id`, `#admin` → `#apply/admin`). Статус мэра: `#apply/mayor/status/:id`.

## Гражданство (public)

- `POST /api/application` — подача (тело JSON + `captcha_token`, опционально `email_already_verified`)
- `GET /api/application/:id` — статус заявки
- `GET /api/public-config` — `recaptcha_site_key` / `recaptcha_required`

Поля: `login`, `full_name`, `passport_data`, `email`, `reason`, captcha.

## Мэрство (Citizen auth)

Публичного apply нет: `POST /api/mayor/apply` на protected-маршрутах.

- `POST /api/mayor/apply` — Bearer API-ключ активного гражданина; тело только `{ "ip": "..." }`
- `GET /api/mayor/status/:id` — публичный статус (`status`, `node_id`, `message`)
- `GET /api/mayor/:id/package` — пакет узла (отдельный auth-шаг, не форма подачи)

Не требуются: captcha, ФИО, motivation. Email в заявку подставляется из реестра `citizens` по ключу.

## reCAPTCHA

Только для гражданства. Сервер проверяет токен через Google `siteverify` (`RECAPTCHA_SECRET_KEY`).

Фронт берёт site key из `window.QUAZAR_RECAPTCHA_SITE_KEY` или из `GET /api/public-config`.

## Логин в форме гражданства

Только латинские буквы `A–Z` / `a–z` (без цифр, пробелов и спецсимволов), без учёта регистра, длина **1–13**. Имя `Successful` зарезервировано.

Админ-панель гражданства: только по hash `#apply/admin` (вкладки в UI нет).
