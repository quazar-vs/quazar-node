-- Apply workflow: citizen email verification + application detection metadata.
ALTER TABLE citizens
  ADD COLUMN IF NOT EXISTS email TEXT,
  ADD COLUMN IF NOT EXISTS email_verified BOOLEAN NOT NULL DEFAULT FALSE,
  ADD COLUMN IF NOT EXISTS verification_code TEXT,
  ADD COLUMN IF NOT EXISTS verification_expires TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_citizens_email_verified
  ON citizens(email_verified)
  WHERE email_verified = FALSE;

ALTER TABLE citizenship_applications
  ADD COLUMN IF NOT EXISTS verification_method TEXT,
  ADD COLUMN IF NOT EXISTS detected_type TEXT;

ALTER TABLE citizenship_applications
  DROP CONSTRAINT IF EXISTS citizenship_applications_verification_method_check;
ALTER TABLE citizenship_applications
  ADD CONSTRAINT citizenship_applications_verification_method_check
  CHECK (verification_method IS NULL OR verification_method IN ('email', 'captcha', 'auto'));

ALTER TABLE citizenship_applications
  DROP CONSTRAINT IF EXISTS citizenship_applications_detected_type_check;
ALTER TABLE citizenship_applications
  ADD CONSTRAINT citizenship_applications_detected_type_check
  CHECK (detected_type IS NULL OR detected_type IN ('human', 'digital'));
