# HANDOFF: Passport — ВПН = принятие роли

Скопируй локальному агенту (`/home/ser/projects/quazar_passport/`).  
На VPS Flutter не править.

## Канон ядра (уже)

1. **Подключение к сети:** `POST /online` `{ "citizen_id": "..." }`  
   → `citizens.status = online` (если паспорт выдан и был pending/online).  
   Экономика ещё закрыта.

2. **Включение ВПН = принятие роли (одно действие):**  
   - Сначала ключ: `POST /api/vpn/key` → в ответе `id` = session.  
   - Затем: `POST /api/citizen/activate_vpn`  
     ```json
     { "vpn_session_id": "<id из /api/vpn/key>" }
     ```  
   - Ответ:  
     ```json
     {
       "status": "active",
       "role": "Страж",
       "role_code": "Guardian",
       "access": ["global_lots", "local_lots", "verify_trades"],
       "idempotent": false
     }
     ```  
   - Повторный вызов безопасен (`idempotent: true`).

3. **Доступ к лотам** (после `active`):  
   - Гражданин → только глобальные.  
   - Страж+ → локальные + подтверждение сделок.

## Что сделать в приложении

1. После логина/подключения к узлу вызывать `POST /online`.  
2. Кнопка «Включить ВПН» должна: выдать/взять VPN key → сразу `activate_vpn` с его `id` (не два разных пользовательских шага «ВПН» и «принять роль»).  
3. Пока `status=online` — не открывать биржу (ядро вернёт 403). Показать CTA «Включите ВПН».  
4. После activate — обновить локальный профиль (`status`, `role`, `access`) и открыть биржу по access.  
5. Не показывать локальные лоты, если в `access` нет `local_lots`.

## Критерии

- [ ] Connect → online  
- [ ] VPN toggle → active + роль одним UX-актом  
- [ ] Биржа только после active  
- [ ] Повторный VPN не ломает профиль  
