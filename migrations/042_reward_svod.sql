-- Государственный Свод Вознаграждений (часть Свода Оснований): ставки КВАЗИ за действия граждан.
-- Table name svod_rewards (not "svod") to avoid clashing with the product name /svod API.

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
        WHERE table_schema = 'public' AND table_name = 'svod'
    ) AND NOT EXISTS (
        SELECT 1 FROM information_schema.tables
        WHERE table_schema = 'public' AND table_name = 'svod_rewards'
    ) THEN
        -- Rename mistaken early table name if present.
        ALTER TABLE svod RENAME TO svod_rewards;
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS svod_rewards (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    action TEXT NOT NULL UNIQUE,
    unit TEXT NOT NULL,
    reward BIGINT NOT NULL CHECK (reward >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_svod_rewards_action ON svod_rewards (action);

CREATE TABLE IF NOT EXISTS channel_videos (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    citizen_id TEXT NOT NULL REFERENCES citizens(id) ON DELETE CASCADE,
    title TEXT NOT NULL DEFAULT '',
    duration_secs INT NOT NULL CHECK (duration_secs >= 15 AND duration_secs <= 30),
    view_count BIGINT NOT NULL DEFAULT 0 CHECK (view_count >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_channel_videos_citizen ON channel_videos (citizen_id);

CREATE TABLE IF NOT EXISTS svod_ledger (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    citizen_id TEXT NOT NULL REFERENCES citizens(id) ON DELETE CASCADE,
    action TEXT NOT NULL,
    units BIGINT NOT NULL,
    amount BIGINT NOT NULL,
    ref_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_svod_ledger_citizen ON svod_ledger (citizen_id);
CREATE INDEX IF NOT EXISTS idx_svod_ledger_ref ON svod_ledger (ref_id);

INSERT INTO svod_rewards (action, unit, reward)
VALUES
    ('video_create', '1 ролик (15–30 секунд)', 50),
    ('video_view', '10 просмотров', 10)
ON CONFLICT (action) DO NOTHING;
