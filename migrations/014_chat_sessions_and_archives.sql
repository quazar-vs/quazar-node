-- Сеансы чата
CREATE TABLE IF NOT EXISTS chat_sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES citizens(id),
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_activity TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    is_active BOOLEAN NOT NULL DEFAULT TRUE
);

-- Уникальный активный сеанс на пользователя
CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_sessions_user_active
    ON chat_sessions(user_id) WHERE is_active = TRUE;

-- Архивы чата
CREATE TABLE IF NOT EXISTS chat_archives (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES chat_sessions(id),
    compressed_data BYTEA NOT NULL,
    original_size INTEGER NOT NULL,
    compressed_size INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    event_id TEXT,
    is_synced BOOLEAN NOT NULL DEFAULT FALSE
);

-- Добавляем session_id в существующую таблицу (сначала nullable)
ALTER TABLE chat_messages
    ADD COLUMN IF NOT EXISTS session_id TEXT REFERENCES chat_sessions(id);

-- Backfill: закрытые сеансы для уже существующих сообщений
DO $$
DECLARE
    r RECORD;
    sid TEXT;
BEGIN
    FOR r IN
        SELECT citizen_id,
               MIN(created_at) AS started,
               MAX(created_at) AS last_act
        FROM chat_messages
        WHERE session_id IS NULL
        GROUP BY citizen_id
    LOOP
        sid := gen_random_uuid()::text;
        INSERT INTO chat_sessions (id, user_id, started_at, last_activity, is_active)
        VALUES (sid, r.citizen_id, r.started, r.last_act, FALSE);

        UPDATE chat_messages
        SET session_id = sid
        WHERE citizen_id = r.citizen_id
          AND session_id IS NULL;
    END LOOP;
END $$;

-- Теперь session_id обязателен
ALTER TABLE chat_messages
    ALTER COLUMN session_id SET NOT NULL;

-- Индексы
CREATE INDEX IF NOT EXISTS idx_chat_sessions_user ON chat_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_chat_sessions_active ON chat_sessions(is_active);
CREATE INDEX IF NOT EXISTS idx_chat_messages_session ON chat_messages(session_id);
CREATE INDEX IF NOT EXISTS idx_chat_archives_synced ON chat_archives(is_synced);
