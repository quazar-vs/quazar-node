-- Portal applications (generic: general / passport / candidacy).
CREATE TABLE IF NOT EXISTS portal_applications (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    category TEXT NOT NULL CHECK (category IN ('general', 'passport', 'candidacy')),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'rejected')),
    citizen_id TEXT REFERENCES citizens(id) ON DELETE SET NULL,
    citizen_name TEXT NOT NULL,
    submitter_ip TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_portal_applications_status
    ON portal_applications (status);
CREATE INDEX IF NOT EXISTS idx_portal_applications_created
    ON portal_applications (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_portal_applications_citizen
    ON portal_applications (citizen_id);
