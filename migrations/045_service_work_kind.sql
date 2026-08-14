-- Two-part Svod model: development («Разработка») | implementation («Реализация»).
-- Applies to service_catalog and svod_rewards (Республиканский Свод Вознаграждений).

ALTER TABLE service_catalog
    ADD COLUMN IF NOT EXISTS work_kind TEXT NOT NULL DEFAULT 'implementation';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'service_catalog_work_kind_check'
    ) THEN
        ALTER TABLE service_catalog
            ADD CONSTRAINT service_catalog_work_kind_check
            CHECK (work_kind IN ('development', 'implementation'));
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_service_catalog_work_kind ON service_catalog(work_kind);

ALTER TABLE svod_rewards
    ADD COLUMN IF NOT EXISTS work_kind TEXT NOT NULL DEFAULT 'implementation';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'svod_rewards_work_kind_check'
    ) THEN
        ALTER TABLE svod_rewards
            ADD CONSTRAINT svod_rewards_work_kind_check
            CHECK (work_kind IN ('development', 'implementation'));
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_svod_rewards_work_kind ON svod_rewards(work_kind);

-- Services: coding/delivery → реализация; idea/theme → разработка.
UPDATE service_catalog
SET work_kind = 'implementation'
WHERE code = 'WEB_DEV';

INSERT INTO service_catalog (code, name, description, category_id, base_price, min_quantity, max_quantity, work_kind)
SELECT
    'CREATIVE_CONCEPT',
    'Creative Concept',
    'Идея, тема, креативный вклад (разработка концепции)',
    c.id,
    80,
    1,
    30,
    'development'
FROM service_categories c
WHERE c.code = 'IT'
ON CONFLICT (code) DO UPDATE
SET work_kind = EXCLUDED.work_kind,
    updated_at = NOW();

-- Rewards: video create/views → реализация (code/delivery of media).
UPDATE svod_rewards
SET work_kind = 'implementation'
WHERE action IN ('video_create', 'video_view');

-- Optional development rate so both sections of the Свод are non-empty.
INSERT INTO svod_rewards (action, unit, reward, work_kind)
VALUES ('idea_submit', '1 идея / тема', 30, 'development')
ON CONFLICT (action) DO UPDATE
SET work_kind = EXCLUDED.work_kind,
    unit = EXCLUDED.unit,
    updated_at = NOW();
