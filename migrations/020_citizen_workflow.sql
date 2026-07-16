-- Citizen onboarding workflow: Applicant status + applications table.
ALTER TABLE citizens DROP CONSTRAINT IF EXISTS citizens_status_check;
ALTER TABLE citizens ADD CONSTRAINT citizens_status_check
  CHECK (status IN ('applicant', 'pending', 'active', 'suspended', 'revoked'));

CREATE TABLE IF NOT EXISTS citizenship_applications (
    id TEXT PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id) ON DELETE CASCADE,
    email TEXT,
    reason TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'Submitted',
    verified_by TEXT,
    verified_at TIMESTAMPTZ,
    approved_by TEXT,
    approved_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (citizen_id)
);

CREATE INDEX IF NOT EXISTS idx_citizenship_applications_status
    ON citizenship_applications(status);
CREATE INDEX IF NOT EXISTS idx_citizens_status_applicant
    ON citizens(status) WHERE status = 'applicant';
