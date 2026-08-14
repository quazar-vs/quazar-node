# HANDOFF: Quazar Passport (Flutter) — Phase A–D

> **Биржа / Свод:** актуальный канон v3 — см. [`passport-exchange-v3-handoff.md`](passport-exchange-v3-handoff.md)  
> и [`passport-svod-exchange-canon-prompt.md`](passport-svod-exchange-canon-prompt.md).  
> Секции биржи ниже частично устарели (две части Свода, confirm только по global/local, экспорт с городом).

Скопируй этот промпт локальному агенту Cursor на ПК  
(репо: `/home/ser/projects/quazar_passport/`).  
**На VPS Flutter не править и APK не собирать.**

---

## Цель
Переделать Passport под новую архитектуру ядра (уже на `api.quazar.website` / VPS) и выпустить установку (APK → GitHub Releases, сайт берёт `PASSPORT_DOWNLOAD_URL`).

## Канон (уже в ядре и законах)

### Статусы vs Мэр
- Статусы: Гость → Кандидат → Гражданин → Страж → Судья → Айя.  
- **Основатель (Successful)** — вне статусов.  
- **Мэр** — **должность узла**, не статус. UI: показывать отдельно «Должность: Мэр» + базовый статус.  
- API: `GET /offices/mayor/:node_id`, `POST /offices/mayor`, `POST /offices/mayor/:node_id/leave`, `POST /city/appoint` `{citizen_id, role: Guardian|Judge, node_id}`.

### Биржа (модель 1B + глобальный/локальный)
- **Глобальные** (в Своде Доступ содержит **Г**): граждане видят/покупают; подтверждает **Агент СБ** автоматически; без перемещения.
- **Локальные** (только Ст/Сд/М): остаток по городу (`GET/POST /exchange/node-lots`).  
  Если в текущем городе 0, а в другом > 0 → строка с `display_status: "Экспорт"`, цена = цена Свода.  
  Пример UI: `Лот: video_create | Цена: 50 КВАЗИ | Доступ: Ст | Статус: Экспорт`
- Экспорт: `POST /exchange/exports` (шаг 1, Страж покупателя) → `POST /exchange/exports/:id/advance {"step":2..9}`.
- `GET /exchange/listings?node_id=...` — витрина города (глобальные + локальные + экспорт).
- `GET /exchange/listings?affordable=true` — только то, что можно оплатить.  
- Потребность: `POST /exchange/needs` `{svod_code, quantity}` → лот республики с **+10%** (глобальные).  
- Покупка глобального: `POST /exchange/listings/:id/buy` — часто сразу `completed` (agent_sb).  
- Локальный/экспорт — живой Страж + протокол.  
- Продажа республике: `POST /exchange/sell-to-state` (−10%), ждёт СБ.  
- Страж: `GET /exchange/security/tasks`, `POST /exchange/security/tasks/:id/confirm`.

### Свод
- Позиции из `GET /svod`; для **экспорта** цена UI = `unit_svod` / `base_price` (без +10%).
- Для глобальных лотов республики UI может показывать `unit_buyer_price` (+10%).

### Онбординг
- Сайт: captcha → email → guest chat → заявка → ссылка на приложение.  
- В приложении: логин по credentials; гражданство/паспорт может оркестрировать Секретарь (`POST /citizen/:id/approve` теперь и у Agent).

## Что сделать в Flutter

1. **Профиль**
   - Статус + отдельный бейдж «Мэр» если `GET /offices/mayor/...` или поле office в citizen (если добавите в API профиля — иначе запрос по node_id).
   - Не показывать Мэра как уровень выше Судьи в иерархии статусов.

2. **Биржа — переработать**
   - Убрать опору на «только Страж» для витрины/покупки.  
   - Витрина: `listings?affordable=true` (+ фильтры work_kind/svod_code).  
   - «Потребность»: выбор позиции Свода → `POST /exchange/needs` → показать `listing_id` и CTA «Открыть сделку».  
   - После buy: экран «Ожидает подтверждения СБ», не «сделка завершена».  
   - «Продать республике»: `sell-to-state` + ожидание СБ.  
   - Страж+: вкладка задач СБ.

3. **Legacy**
   - Старые `/exchange/offer` / `order` можно оставить как fallback, но основной UX — needs / listings / sell-to-state / security.

4. **Тексты**
   - Согласовать с KVA/KGPS: гражданин может биржу; СБ-диалог — Страж+.

## Сборка и установка

1. Собрать release APK локально (`flutter build apk`).  
2. Выложить в `quazar-vs/quazar-passport` Releases (или обновить asset).  
3. На VPS уже: `PASSPORT_DOWNLOAD_URL=https://github.com/quazar-vs/quazar-passport/releases` — сайт подхватит.  
4. Если нужна прямая ссылка на файл APK — обновить `PASSPORT_DOWNLOAD_URL` в `/opt/quazar/.env` и перезапустить node (это сделает **агент на VPS** по запросу).

## Acceptance

- [ ] Гражданин видит только affordable лоты и может открыть сделку.  
- [ ] После buy — «ожидает СБ», не completed.  
- [ ] Потребность → лот → buy работает end-to-end против prod API.  
- [ ] Мэр отображается как должность, не как статус в лестнице.  
- [ ] Страж видит и подтверждает security tasks.  
- [ ] Новый APK в Releases; с сайта качается актуальная сборка.

## API base
Prod: `https://api.quazar.website` (или тот URL, что уже в приложении).  
Auth: Bearer API key гражданина.
