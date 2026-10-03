-- Trust plane vs reachability: node identity is ID (+ pubkey), not a fixed URL.
-- Dynamic IP lives in node_endpoints (soft-state), not as UNIQUE personality.

ALTER TABLE nodes DROP CONSTRAINT IF EXISTS nodes_url_key;

ALTER TABLE nodes ALTER COLUMN url DROP NOT NULL;
ALTER TABLE nodes ALTER COLUMN url SET DEFAULT '';

UPDATE nodes SET url = COALESCE(url, '');

COMMENT ON TABLE nodes IS
  'Trusted republic nodes by ID; url is optional legacy/bootstrap hint, not identity';

CREATE TABLE IF NOT EXISTS node_endpoints (
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    endpoint TEXT NOT NULL,
    last_seen TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    source TEXT NOT NULL DEFAULT 'announce',
    PRIMARY KEY (node_id, endpoint)
);

CREATE INDEX IF NOT EXISTS idx_node_endpoints_seen
    ON node_endpoints (node_id, last_seen DESC);

COMMENT ON TABLE node_endpoints IS
  'Ephemeral reachability tips (dynamic IP); not part of trust/canon identity';
