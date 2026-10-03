# Прописка и пропуски городов

## Модель

| Понятие | Где | Смысл |
|---------|-----|--------|
| **Прописка** | `citizens.home_node_id` + `passports.home_node_id` | узел регистрации; не меняется без отдельной процедуры |
| **Пропуск home** | `city_passes` (`kind=home`) | автоматически при выдаче паспорта на узел прописки |
| **Пропуск visit** | `city_passes` (`kind=visit`) | вход в чужой город; выдаёт СБ/Мэр того города |
| **online** | `citizens.status` | присутствие / сессия, не прописка |

## API

| Метод | Путь | Кто |
|-------|------|-----|
| GET | `/citizen/:id/passport` | сам гражданин / СБ / судья / manage |
| POST | `/city/:node_id/passes` | Guardian/Judge/Aiya/Founder или Мэр этого `node_id` — body `{ citizen_id, expires_in_days? }` |
| POST | `/city/:node_id/passes/:pass_id/revoke` | те же |
| GET | `/city/:node_id/access/:citizen_id` | проверка доступа |

Ответ паспорта включает `home_node_id` и массив `passes`.
