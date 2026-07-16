-- Distinguish human vs digital agents.
ALTER TABLE citizens
  ADD COLUMN IF NOT EXISTS is_digital BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE citizenship_applications
  ADD COLUMN IF NOT EXISTS applicant_type TEXT NOT NULL DEFAULT 'human';

ALTER TABLE citizenship_applications DROP CONSTRAINT IF EXISTS citizenship_applications_applicant_type_check;
ALTER TABLE citizenship_applications ADD CONSTRAINT citizenship_applications_applicant_type_check
  CHECK (applicant_type IN ('human', 'digital'));

ALTER TABLE citizenship_applications DROP CONSTRAINT IF EXISTS citizenship_applications_status_check;
ALTER TABLE citizenship_applications ADD CONSTRAINT citizenship_applications_status_check
  CHECK (status IN ('Submitted', 'Verified', 'Approved', 'Rejected'));

-- Keep citizens status values explicit (same set as 020).
ALTER TABLE citizens DROP CONSTRAINT IF EXISTS citizens_status_check;
ALTER TABLE citizens ADD CONSTRAINT citizens_status_check
  CHECK (status IN ('applicant', 'pending', 'active', 'suspended', 'revoked'));
