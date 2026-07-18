# Портал заявок на гражданство — Квазар

Однофайловый фронт: `applications/index.html`.

## API (public)

- `POST /api/application` — подача заявки (тело JSON + `captcha_token`)
- `POST /api/application/verify` — подтверждение email-кода
- `GET /api/application/:id` — статус заявки
- `GET /api/public-config` — `recaptcha_site_key` / `recaptcha_required`

## reCAPTCHA

Сервер проверяет токен через Google `siteverify` (`RECAPTCHA_SECRET_KEY`).

Фронт берёт site key из `window.QUAZAR_RECAPTCHA_SITE_KEY` или из `GET /api/public-config`.

```bash
# .env
RECAPTCHA_SITE_KEY=...
RECAPTCHA_SECRET_KEY=...
# за reverse-proxy (локальный nginx; пиры из `nodes` подхватываются автоматически):
QUAZAR_TRUSTED_PROXIES=127.0.0.1,::1
```

## Логин

Только латинские буквы `A–Z` / `a–z` (без цифр, пробелов и спецсимволов), без учёта регистра, длина **1–13**. Имя `Successful` зарезервировано.

## Конфиг фронта

```html
<script>
  window.QUAZAR_API_BASE = "http://127.0.0.1:8080";
  // опционально, если не используете /api/public-config:
  // window.QUAZAR_RECAPTCHA_SITE_KEY = "6Lc...";
</script>
```

## Локальный просмотр

Откройте файл в браузере или отдайте через nginx рядом с основным лендингом.
