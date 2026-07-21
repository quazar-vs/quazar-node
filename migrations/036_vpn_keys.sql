CREATE TABLE IF NOT EXISTS vpn_keys (
    id TEXT PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    private_key TEXT NOT NULL,
    public_key TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    last_used TIMESTAMPTZ,
    is_active BOOLEAN DEFAULT TRUE,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX idx_vpn_keys_citizen ON vpn_keys(citizen_id, is_active);
CREATE INDEX idx_vpn_keys_expires ON vpn_keys(expires_at);
