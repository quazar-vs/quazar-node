-- Street (public website) mayor applications: no citizen yet, review like citizenship.
-- Keep existing citizen-path rows intact (citizen_id / node_ip remain set).

ALTER TABLE mayor_applications
    ALTER COLUMN citizen_id DROP NOT NULL;

ALTER TABLE mayor_applications
    ALTER COLUMN node_ip DROP NOT NULL;

ALTER TABLE mayor_applications
    ADD COLUMN IF NOT EXISTS login TEXT,
    ADD COLUMN IF NOT EXISTS full_name TEXT,
    ADD COLUMN IF NOT EXISTS reason TEXT,
    ADD COLUMN IF NOT EXISTS confirmed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS review_note TEXT,
    ADD COLUMN IF NOT EXISTS reviewed_by TEXT,
    ADD COLUMN IF NOT EXISTS reviewed_at TIMESTAMPTZ;

-- Allow rejected (admin review) alongside node lifecycle statuses.
ALTER TABLE mayor_applications
    DROP CONSTRAINT IF EXISTS mayor_applications_status_check;

ALTER TABLE mayor_applications
    ADD CONSTRAINT mayor_applications_status_check
    CHECK (status IN ('pending', 'approved', 'confirmed', 'timed_out', 'rejected'));

-- One active street application per email (citizen_id IS NULL).
CREATE UNIQUE INDEX IF NOT EXISTS idx_mayor_applications_street_email_active
    ON mayor_applications (lower(external_email))
    WHERE citizen_id IS NULL
      AND external_email IS NOT NULL
      AND status IN ('pending', 'confirmed');

-- One active street application per login.
CREATE UNIQUE INDEX IF NOT EXISTS idx_mayor_applications_street_login_active
    ON mayor_applications (lower(login))
    WHERE citizen_id IS NULL
      AND login IS NOT NULL
      AND status IN ('pending', 'confirmed');
