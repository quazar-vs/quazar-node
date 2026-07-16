-- Replace plaintext api_keys (003) with hashed keys bound to citizens.
DROP TABLE IF EXISTS api_keys;

CREATE TABLE api_keys (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    citizen_id TEXT NOT NULL REFERENCES citizens(id) ON DELETE CASCADE,
    key_hash TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    last_used TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_api_keys_citizen_id ON api_keys (citizen_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_key_hash ON api_keys (key_hash);
CREATE INDEX IF NOT EXISTS idx_api_keys_is_active ON api_keys (is_active);
