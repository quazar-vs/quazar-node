# Quazar mail (Postfix + Dovecot)

Лёгкий почтовый сервер в Docker вместо `docker-mailserver`.

## Запуск

```bash
cd /opt/quazar
docker compose up -d --build mailserver
docker compose ps mailserver   # должен быть healthy
```

## CLI (совместим с MAIL_AUTO_CREATE)

```bash
docker exec quazar-mailserver-1 setup email add user@quazar.website 'StrongPassword16!'
docker exec quazar-mailserver-1 setup email list
docker exec quazar-mailserver-1 setup email del user@quazar.website
```

При старте, если задан `SMTP_PASSWORD` в `.env`, создаётся ящик `MAIL_FROM` / `POSTMASTER` (по умолчанию `no-reply@quazar.website`).

## Порты

| Порт | Назначение |
|------|------------|
| 25   | SMTP |
| 587  | Submission (STARTTLS) |
| 465  | SMTPS |
| 143  | IMAP |
| 993  | IMAPS |

TLS: self-signed (для внутренней сети / `SMTP_TLS_INSECURE=true`). Для продакшена позже можно подставить Let's Encrypt в volume `quazar_mail_ssl`.

## Связь с quazar-node

- `SMTP_HOST=mailserver`
- `MAIL_CONTAINER=quazar-mailserver-1`
- `docker.sock` смонтирован в `quazar-node` для `docker exec … setup email add`
