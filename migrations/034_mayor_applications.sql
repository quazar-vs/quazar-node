-- Mayor node applications (pending → approved → confirmed / timed_out)
-- expires_at: application window is 24 hours from creation
CREATE TABLE IF NOT EXISTS mayor_applications (
    id UUID PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    node_ip TEXT NOT NULL,
    sync_peer TEXT,
    node_id TEXT,
    node_secret TEXT,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'confirmed', 'timed_out')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (NOW() + INTERVAL '24 hours'),
    package_delivered_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_mayor_applications_citizen_id
    ON mayor_applications (citizen_id);

CREATE INDEX IF NOT EXISTS idx_mayor_applications_status
    ON mayor_applications (status);

CREATE INDEX IF NOT EXISTS idx_mayor_applications_expires_at
    ON mayor_applications (expires_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_mayor_applications_node_ip_active
    ON mayor_applications (node_ip)
    WHERE status IN ('pending', 'approved', 'confirmed');

CREATE UNIQUE INDEX IF NOT EXISTS idx_mayor_applications_citizen_active
    ON mayor_applications (citizen_id)
    WHERE status IN ('pending', 'approved', 'confirmed');
