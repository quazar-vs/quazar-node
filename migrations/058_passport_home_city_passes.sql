-- Passport registration (прописка) + city passes (пропуски).
-- Home city pass is auto-issued with the passport; visit passes are issued by city SB/Mayor.

ALTER TABLE citizens
    ADD COLUMN IF NOT EXISTS home_node_id TEXT;

ALTER TABLE passports
    ADD COLUMN IF NOT EXISTS home_node_id TEXT;

CREATE TABLE IF NOT EXISTS city_passes (
    id TEXT PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id) ON DELETE CASCADE,
    passport_id TEXT NOT NULL REFERENCES passports(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('home', 'visit')),
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'revoked', 'expired')),
    issued_by TEXT NOT NULL,
    issued_at BIGINT NOT NULL,
    expires_at BIGINT,
    revoked_at BIGINT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_city_passes_one_active
    ON city_passes (citizen_id, node_id)
    WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_city_passes_citizen
    ON city_passes (citizen_id);

CREATE INDEX IF NOT EXISTS idx_city_passes_node
    ON city_passes (node_id)
    WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_citizens_home_node
    ON citizens (home_node_id)
    WHERE home_node_id IS NOT NULL;

-- Legacy citizens: прописка на первом/республиканском узле.
UPDATE citizens
SET home_node_id = 'QZ-NODE-1'
WHERE passport_issued = TRUE
  AND (home_node_id IS NULL OR btrim(home_node_id) = '');

UPDATE passports p
SET home_node_id = COALESCE(
    (
        SELECT c.home_node_id
        FROM citizens c
        WHERE c.id = p.citizen_id
          AND c.home_node_id IS NOT NULL
          AND btrim(c.home_node_id) <> ''
    ),
    'QZ-NODE-1'
)
WHERE p.home_node_id IS NULL OR btrim(p.home_node_id) = '';

INSERT INTO city_passes (
    id, citizen_id, passport_id, node_id, kind, status, issued_by, issued_at, expires_at
)
SELECT
    'pass_home_' || p.id,
    p.citizen_id,
    p.id,
    p.home_node_id,
    'home',
    'active',
    'backfill',
    p.issued_at,
    p.expires_at
FROM passports p
WHERE p.is_valid = TRUE
  AND p.home_node_id IS NOT NULL
  AND btrim(p.home_node_id) <> ''
  AND NOT EXISTS (
      SELECT 1 FROM city_passes cp
      WHERE cp.citizen_id = p.citizen_id
        AND cp.node_id = p.home_node_id
        AND cp.status = 'active'
  );
