-- Одна глобальная room-сессия для общего чата.
-- Все живые сообщения переносятся в один активный сеанс (user_id = quazar_chat),
-- чтобы архивация давала один ChatHistoryAdded на период комнаты.

-- Владелец room-сессии (FK chat_sessions.user_id → citizens)
INSERT INTO citizens (id, name, public_key, status, role, created_at, passport_issued)
VALUES (
    'quazar_chat',
    'Общий чат',
    '0000000000000000000000000000000000000000000000000000000000000001',
    'active',
    'Citizen',
    EXTRACT(EPOCH FROM NOW())::BIGINT,
    false
)
ON CONFLICT (id) DO NOTHING;

DO $$
DECLARE
    room_sid TEXT;
    min_started TIMESTAMPTZ;
    max_activity TIMESTAMPTZ;
    msg_count INT;
BEGIN
    -- Already migrated?
    SELECT id INTO room_sid
    FROM chat_sessions
    WHERE user_id = 'quazar_chat' AND is_active = TRUE
    LIMIT 1;

    SELECT COUNT(*)::INT, MIN(created_at), MAX(created_at)
    INTO msg_count, min_started, max_activity
    FROM chat_messages;

    IF room_sid IS NULL AND msg_count > 0 THEN
        room_sid := gen_random_uuid()::text;

        INSERT INTO chat_sessions (id, user_id, started_at, last_activity, is_active)
        VALUES (
            room_sid,
            'quazar_chat',
            COALESCE(min_started, NOW()),
            COALESCE(max_activity, NOW()),
            TRUE
        );
    END IF;

    IF room_sid IS NOT NULL AND msg_count > 0 THEN
        UPDATE chat_messages SET session_id = room_sid WHERE session_id IS DISTINCT FROM room_sid;

        UPDATE chat_sessions
        SET last_activity = GREATEST(last_activity, COALESCE(max_activity, last_activity))
        WHERE id = room_sid;

        UPDATE chat_sessions
        SET is_active = FALSE
        WHERE id <> room_sid AND is_active = TRUE;
    ELSIF room_sid IS NULL THEN
        -- No live messages: just close leftover per-user active sessions.
        UPDATE chat_sessions SET is_active = FALSE WHERE is_active = TRUE;
    ELSE
        -- Room exists, no messages (or already remapped): close non-room actives.
        UPDATE chat_sessions
        SET is_active = FALSE
        WHERE is_active = TRUE AND user_id <> 'quazar_chat';
    END IF;
END $$;

-- Индекс «один активный сеанс на user_id» из 014 сохраняем:
-- дальше активной будет только сессия quazar_chat.
