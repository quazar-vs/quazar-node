-- Portal citizenship applications
-- Table: applications (pending -> confirmed -> approved/rejected)

CREATE TABLE IF NOT EXISTS applications (
    id UUID PRIMARY KEY,

    login TEXT NOT NULL UNIQUE,
    full_name TEXT NOT NULL,
    passport_data TEXT NOT NULL,
    email TEXT NOT NULL,
    reason TEXT NOT NULL,

    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'confirmed', 'approved', 'rejected')),

    -- SHA-256 hex hash of the 6-digit verification code
    verification_code TEXT NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    confirmed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_applications_status
    ON applications (status);
CREATE INDEX IF NOT EXISTS idx_applications_created_at
    ON applications (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_applications_email_lower
    ON applications (lower(email));

