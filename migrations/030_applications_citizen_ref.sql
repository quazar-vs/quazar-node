ALTER TABLE applications
    ADD COLUMN IF NOT EXISTS citizen_id TEXT REFERENCES citizens(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_applications_citizen_id
    ON applications (citizen_id);

