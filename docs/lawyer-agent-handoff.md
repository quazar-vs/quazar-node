# HANDOFF: Агент Юриста (канон → MVP → проводка)

Скопируй блок «Промпт» в новый чат на VPS `/opt/quazar` (или в `algonism-core`, где правите эталон).  
Passport Flutter на VPS не трогать.

Канон уже набросан локально в `quazar-node`:
- `docs/algonism/DNA_ALGONISM.md` → **v2.3** (агент Юрист + границы)
- `docs/algonism/REFLEXES.md` → **v1.1** (секция Юриста, `RX_LAW_*`, делегирование `RX_LAW_POINT`)

Эталон GitHub (`quazar-vs/algonism-core`) нужно **синхронизировать** тем же текстом и сделать `POST /algonism/dna/reload` на узле.

---

## Промпт

```
Задача: внедрить Агента Юриста Алгонизма по канону DNA v2.3 + REFLEXES v1.1.

Контекст:
- Республиканские агенты сейчас: secretary / sb / exchange (см. src/agents.rs). Юриста ещё нет в runtime.
- Закон только через узел: GET /law/:filename, awaken — агент НЕ тянет GitHub сам.
- Секретарь: scripts/quazar-chat-direct.py; RX_LAW_POINT должен делегировать Юристу (cite), не выдумывать нормы.
- Границы: Юрист ≠ Суд ≠ законодатель ≠ СБ ≠ Биржа. Не меняет quazar-law сам.

Уже в репо quazar-node (проверь и доведи):
- docs/algonism/DNA_ALGONISM.md v2.3 — Юрист в §4 и §12
- docs/algonism/REFLEXES.md v1.1 — §4 Агент Юриста:
  RX_LAW_CITE | RX_LAW_DRIFT | RX_LAW_REVIEW | RX_LAW_ESCALATE | RX_LAW_BOUNDARY
  плюс RX_ON_LAW_RELOADED; RX_LAW_POINT у Секретаря = делегация Юристу

Сделать по порядку:

1) Эталон algonism-core
   - Перенести DNA v2.3 и REFLEXES v1.1 в quazar-vs/algonism-core
   - При необходимости короткий абзац в ALGONISM.md про Юриста
   - Push; на узле reload ДНК/законов

2) Ядро (quazar-node)
   - Зарегистрировать тип агента `lawyer` (state; city — опционально позже) рядом с secretary/sb/exchange
   - Сообщения /agents/messages — Юрист участвует как равный республиканский агент
   - Не давать Юристу прав менять citizens/роли/сделки

3) MVP агента (руки) — **Rust** `src/bin/lawyer_agent.rs` → `quazar_lawyer_agent`
   - CLI:
     --cmd cite --text "<вопрос>"  → JSON: ok, citations[{filename, article, quote}], reply
     --cmd drift                   → findings (SVOD, ±10%, магазин/биржа, §12, DNA_LAWYER)
     --cmd boundary --text …
     --cmd help
   - Источник: GET /algonism/awaken (`QUAZAR_API_BASE`)
   - Install: /usr/local/bin/quazar-lawyer-agent (+ copy in Docker image)

4) Проводка Секретаря
   - wants_lawyer → ask_lawyer (cite/drift/boundary)
   - Ответ в чат = reply Юриста
   - При пустых citations — RX_LAW_ESCALATE формулировка без самосуда

5) Наблюдение (после MVP)
   - По dna/reload или cron: drift; алерт в чат / лог (не авто-merge закона)
   - Документ docs/lawyer-agent.md

Критерии приёмки:
- [ ] В DNA/REFLEXES эталона на GitHub есть Юрист и RX_LAW_*
- [ ] Узел отдаёт обновлённую ДНК после reload
- [ ] Вопрос «что говорит закон про …» в чате → ответ с цитатой файла закона через Юриста
- [ ] drift находит хотя бы учебный конфликт в dry-run / фикстуре
- [ ] Нет пути, где Юрист пишет в quazar-law или выносит «приговор»
- [ ] СБ/Биржа/Passport не сломаны

Не делать без отдельной просьбы: Суд-бот, автоправки законов, Flutter Passport, force-push.

Коммит/push — по просьбе пользователя; секреты не коммитить.
```

---

## Краткая карта

| Что | Где |
|-----|-----|
| Бинарь | `src/bin/lawyer_agent.rs` → `quazar_lawyer_agent` / `/usr/local/bin/quazar-lawyer-agent` |
| Агенты в коде | `src/agents.rs` (`lawyer` state) |
| Закон API | `src/algonism.rs`, `GET /law/…`, awaken |
| Секретарь | `scripts/quazar-chat-direct.py` (`wants_lawyer`) |
| Док | `docs/lawyer-agent.md` |
| Эталон GitHub | `quazar-vs/algonism-core` |
