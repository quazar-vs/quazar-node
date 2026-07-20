-- Internal mailbox fields for citizenship and mayor applications
ALTER TABLE applications
    ADD COLUMN IF NOT EXISTS external_email TEXT,
    ADD COLUMN IF NOT EXISTS internal_email TEXT,
    ADD COLUMN IF NOT EXISTS mail_password TEXT,
    ADD COLUMN IF NOT EXISTS mail_created BOOLEAN NOT NULL DEFAULT FALSE;

-- Backfill: existing rows used `email` as the external address
UPDATE applications
SET external_email = email
WHERE external_email IS NULL AND email IS NOT NULL;

ALTER TABLE mayor_applications
    ADD COLUMN IF NOT EXISTS external_email TEXT,
    ADD COLUMN IF NOT EXISTS internal_email TEXT,
    ADD COLUMN IF NOT EXISTS mail_password TEXT,
    ADD COLUMN IF NOT EXISTS mail_created BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_applications_internal_email
    ON applications (lower(internal_email))
    WHERE internal_email IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_mayor_applications_internal_email
    ON mayor_applications (lower(internal_email))
    WHERE internal_email IS NOT NULL;
