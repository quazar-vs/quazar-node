-- Unified candidates table: citizenship / mayor / role applications.
-- Legacy tables (applications, mayor_applications, candidacies) remain for
-- node-lifecycle and vote flows; they stay dual-written from the API layer.

CREATE TABLE IF NOT EXISTS candidates (
    id UUID PRIMARY KEY,
    kind TEXT NOT NULL
        CHECK (kind IN ('citizenship', 'mayor', 'role')),
    status TEXT NOT NULL,
    email TEXT,
    display_name TEXT,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    source TEXT,
    citizen_id TEXT REFERENCES citizens(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    decided_at TIMESTAMPTZ,
    decided_by TEXT
);

CREATE INDEX IF NOT EXISTS idx_candidates_status
    ON candidates (status);

CREATE INDEX IF NOT EXISTS idx_candidates_email_lower
    ON candidates (lower(email))
    WHERE email IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_candidates_kind_status
    ON candidates (kind, status);

CREATE INDEX IF NOT EXISTS idx_candidates_citizen_id
    ON candidates (citizen_id)
    WHERE citizen_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_candidates_created_at
    ON candidates (created_at DESC);

-- One active street/portal application per email+kind
-- (role candidacies use Active/Approved/… and are keyed by citizen).
CREATE UNIQUE INDEX IF NOT EXISTS idx_candidates_email_kind_active
    ON candidates (lower(email), kind)
    WHERE email IS NOT NULL
      AND status IN ('pending', 'confirmed');

-- ---------------------------------------------------------------------------
-- Data backfill
-- ---------------------------------------------------------------------------

-- Citizenship applications → candidates(kind=citizenship)
-- If multiple pending/confirmed share an email, keep the newest as "active"
-- and demote older actives to rejected in candidates only (legacy row untouched).
INSERT INTO candidates (
    id, kind, status, email, display_name, payload, source,
    citizen_id, created_at, updated_at, decided_at, decided_by
)
SELECT
    a.id,
    'citizenship',
    CASE
        WHEN a.status IN ('pending', 'confirmed')
             AND a.rn > 1 THEN 'rejected'
        ELSE a.status
    END,
    a.email,
    COALESCE(NULLIF(a.full_name, ''), a.login),
    jsonb_strip_nulls(jsonb_build_object(
        'login', a.login,
        'full_name', a.full_name,
        'passport_data', a.passport_data,
        'reason', a.reason,
        'verification_code', a.verification_code,
        'confirmed_at', a.confirmed_at,
        'review_note', a.review_note,
        'reviewed_by', a.reviewed_by,
        'reviewed_at', a.reviewed_at,
        'external_email', a.external_email,
        'legacy_table', 'applications',
        'demoted_duplicate_active', (a.status IN ('pending', 'confirmed') AND a.rn > 1)
    )),
    'portal',
    a.citizen_id,
    a.created_at,
    a.updated_at,
    a.reviewed_at,
    a.reviewed_by
FROM (
    SELECT
        applications.*,
        ROW_NUMBER() OVER (
            PARTITION BY lower(email),
                         CASE WHEN status IN ('pending', 'confirmed') THEN 1 ELSE 0 END
            ORDER BY created_at DESC
        ) AS rn
    FROM applications
) a
ON CONFLICT (id) DO NOTHING;

-- Mayor applications → candidates(kind=mayor)
INSERT INTO candidates (
    id, kind, status, email, display_name, payload, source,
    citizen_id, created_at, updated_at, decided_at, decided_by
)
SELECT
    m.id,
    'mayor',
    CASE
        WHEN m.status IN ('pending', 'confirmed')
             AND m.external_email IS NOT NULL
             AND m.rn > 1 THEN 'rejected'
        ELSE m.status
    END,
    m.external_email,
    COALESCE(
        NULLIF(m.full_name, ''),
        NULLIF(m.login, ''),
        m.citizen_id
    ),
    jsonb_strip_nulls(jsonb_build_object(
        'login', m.login,
        'full_name', m.full_name,
        'passport_data', m.passport_data,
        'reason', m.reason,
        'node_ip', m.node_ip,
        'sync_peer', m.sync_peer,
        'node_id', m.node_id,
        'node_secret', m.node_secret,
        'expires_at', m.expires_at,
        'package_delivered_at', m.package_delivered_at,
        'confirmed_at', m.confirmed_at,
        'review_note', m.review_note,
        'reviewed_by', m.reviewed_by,
        'reviewed_at', m.reviewed_at,
        'external_email', m.external_email,
        'street', (m.citizen_id IS NULL AND m.login IS NOT NULL),
        'legacy_table', 'mayor_applications',
        'demoted_duplicate_active', (
            m.status IN ('pending', 'confirmed')
            AND m.external_email IS NOT NULL
            AND m.rn > 1
        )
    )),
    CASE
        WHEN m.citizen_id IS NULL AND m.login IS NOT NULL THEN 'street'
        WHEN m.citizen_id IS NOT NULL THEN 'citizen'
        ELSE 'portal'
    END,
    m.citizen_id,
    m.created_at,
    COALESCE(m.reviewed_at, m.created_at),
    m.reviewed_at,
    m.reviewed_by
FROM (
    SELECT
        mayor_applications.*,
        ROW_NUMBER() OVER (
            PARTITION BY lower(external_email),
                         CASE WHEN status IN ('pending', 'confirmed') THEN 1 ELSE 0 END
            ORDER BY created_at DESC
        ) AS rn
    FROM mayor_applications
    WHERE external_email IS NOT NULL
) m
ON CONFLICT (id) DO NOTHING;

-- Mayor rows without email (citizen-path / incomplete) — no active unique conflict.
INSERT INTO candidates (
    id, kind, status, email, display_name, payload, source,
    citizen_id, created_at, updated_at, decided_at, decided_by
)
SELECT
    m.id,
    'mayor',
    m.status,
    NULL,
    COALESCE(NULLIF(m.full_name, ''), NULLIF(m.login, ''), m.citizen_id),
    jsonb_strip_nulls(jsonb_build_object(
        'login', m.login,
        'full_name', m.full_name,
        'passport_data', m.passport_data,
        'reason', m.reason,
        'node_ip', m.node_ip,
        'sync_peer', m.sync_peer,
        'node_id', m.node_id,
        'node_secret', m.node_secret,
        'expires_at', m.expires_at,
        'package_delivered_at', m.package_delivered_at,
        'confirmed_at', m.confirmed_at,
        'review_note', m.review_note,
        'reviewed_by', m.reviewed_by,
        'reviewed_at', m.reviewed_at,
        'street', false,
        'legacy_table', 'mayor_applications'
    )),
    CASE WHEN m.citizen_id IS NOT NULL THEN 'citizen' ELSE 'portal' END,
    m.citizen_id,
    m.created_at,
    COALESCE(m.reviewed_at, m.created_at),
    m.reviewed_at,
    m.reviewed_by
FROM mayor_applications m
WHERE m.external_email IS NULL
ON CONFLICT (id) DO NOTHING;

-- Role candidacies → candidates(kind=role); id is TEXT UUID from nominate flow
INSERT INTO candidates (
    id, kind, status, email, display_name, payload, source,
    citizen_id, created_at, updated_at, decided_at, decided_by
)
SELECT
    c.id::uuid,
    'role',
    c.status,
    cit.email,
    COALESCE(cit.name, c.citizen_id),
    jsonb_strip_nulls(jsonb_build_object(
        'target_role', c.target_role,
        'votes_for', c.votes_for,
        'votes_against', c.votes_against,
        'threshold', c.threshold,
        'nominator_id', c.nominator_id,
        'approved_at', c.approved_at,
        'appointed_at', c.appointed_at,
        'legacy_table', 'candidacies'
    )),
    'candidacy',
    c.citizen_id,
    c.created_at,
    COALESCE(c.appointed_at, c.approved_at, c.created_at),
    COALESCE(c.appointed_at, c.approved_at),
    NULL
FROM candidacies c
LEFT JOIN citizens cit ON cit.id = c.citizen_id
WHERE c.id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
ON CONFLICT (id) DO NOTHING;
