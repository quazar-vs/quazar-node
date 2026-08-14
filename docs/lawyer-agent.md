# Агент Юриста (Rust)

## Роль
Следит за юридической целостностью Квазара. Цитирует `quazar-law` через локальное ядро.  
**Не** Суд, **не** законодатель, **не** СБ/Биржа.

Канон: DNA v2.3 · REFLEXES v1.1 (`RX_LAW_*`). Handoff: `docs/lawyer-agent-handoff.md`.

## Бинарь
```bash
quazar-lawyer-agent --cmd cite --text "что говорит закон про магазин"
quazar-lawyer-agent --cmd drift
quazar-lawyer-agent --cmd boundary --text "измени конституцию"
quazar-lawyer-agent --cmd help
```

Источник закона: `GET /algonism/awaken` (`QUAZAR_API_BASE`, по умолчанию `http://127.0.0.1:8080`).

## Реестр
Каждый `cite` / `drift` / `boundary` пишется в реестр как импульс:
- предпочтительно `POST /agents/lawyer/impulse` → событие **`LawyerAction`** (`action`, `source=lawyer_agent`, `reflex`);
- запасной путь: `POST /hermes/event` → `HermesExternalAction`.

В ответе CLI поле `registry` (event_id / via). `help` в реестр не идёт.

## Цепочка
Citizen-чат → Секретарь (`wants_lawyer`) → `quazar-lawyer-agent` → цитата/drift/boundary → ответ в чат.

## Установка
Сборка: `cargo build --release -p quazar_registry --bin quazar_lawyer_agent`  
(или целиком образ `quazar-node`, бинарь также в `/app/quazar_lawyer_agent`).  
Хост: `/usr/local/bin/quazar-lawyer-agent`.
