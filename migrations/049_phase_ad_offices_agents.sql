-- Phase C: unify treasury into Successful reserve (QZ-1); offices + agents scaffolding.

-- Merge legacy treasury `quazar` balance into QZ-1.
INSERT INTO balances (citizen_id, amount)
VALUES ('QZ-1', 0)
ON CONFLICT (citizen_id) DO NOTHING;

UPDATE balances AS q
SET amount = q.amount + COALESCE((SELECT amount FROM balances WHERE citizen_id = 'quazar'), 0)
WHERE q.citizen_id = 'QZ-1';

UPDATE balances SET amount = 0 WHERE citizen_id = 'quazar';

-- Mayor as temporary office (not a status).
CREATE TABLE IF NOT EXISTS node_offices (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    office TEXT NOT NULL DEFAULT 'mayor',
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    previous_role TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    started_at BIGINT NOT NULL,
    ended_at BIGINT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_node_offices_one_active
    ON node_offices (node_id, office)
    WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_node_offices_citizen ON node_offices (citizen_id);
CREATE INDEX IF NOT EXISTS idx_node_offices_active ON node_offices (status) WHERE status = 'active';

-- Typed Algonism agents (state + city).
CREATE TABLE IF NOT EXISTS algonism_agents (
    id TEXT PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    agent_kind TEXT NOT NULL CHECK (agent_kind IN ('secretary', 'sb', 'exchange')),
    scope TEXT NOT NULL CHECK (scope IN ('state', 'city')),
    city_id TEXT,
    node_id TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    created_at BIGINT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_algonism_agents_unique
    ON algonism_agents (agent_kind, scope, COALESCE(city_id, ''), COALESCE(node_id, ''));

CREATE INDEX IF NOT EXISTS idx_algonism_agents_kind ON algonism_agents (agent_kind, scope);

CREATE TABLE IF NOT EXISTS agent_messages (
    id TEXT PRIMARY KEY,
    from_agent_id TEXT NOT NULL REFERENCES algonism_agents(id),
    to_agent_id TEXT NOT NULL REFERENCES algonism_agents(id),
    body TEXT NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_agent_messages_to ON agent_messages (to_agent_id, created_at DESC);
