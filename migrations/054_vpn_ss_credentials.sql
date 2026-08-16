-- Ephemeral Shadowsocks credentials for citizen online VPN (replaces SSH tunnel keys for Passport).
-- Legacy table vpn_keys is retained but the node no longer issues SSH keys into it.

CREATE TABLE IF NOT EXISTS vpn_ss_credentials (
    id TEXT PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    password TEXT NOT NULL,
    method TEXT NOT NULL DEFAULT 'chacha20-ietf-poly1305',
    port INT NOT NULL,
    acl_profile TEXT NOT NULL DEFAULT 'citizen_youtube_telegram',
    expires_at TIMESTAMPTZ NOT NULL,
    last_used TIMESTAMPTZ,
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_vpn_ss_credentials_citizen
    ON vpn_ss_credentials(citizen_id, is_active);
CREATE INDEX IF NOT EXISTS idx_vpn_ss_credentials_expires
    ON vpn_ss_credentials(expires_at);
CREATE INDEX IF NOT EXISTS idx_vpn_ss_credentials_active
    ON vpn_ss_credentials(is_active) WHERE is_active = TRUE;
