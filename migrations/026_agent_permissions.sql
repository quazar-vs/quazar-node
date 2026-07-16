-- Individual Agent permissions (deny-by-default).
-- Known permission strings:
--   chat.read        — read chat messages / session / history / search
--   chat.send        — send chat messages
--   chat.manage      — chat moderation (admin actions); normally reserved for Founder/Aiya
--   blockchain.read  — reserved for future Agent blockchain read access
CREATE TABLE IF NOT EXISTS agent_permissions (
  id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL REFERENCES citizens(id) ON DELETE CASCADE,
  permission TEXT NOT NULL,
  granted_by TEXT,
  granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (agent_id, permission),
  CONSTRAINT agent_permissions_permission_check
    CHECK (permission IN (
      'chat.read',
      'chat.send',
      'chat.manage',
      'blockchain.read'
    ))
);

CREATE INDEX IF NOT EXISTS idx_agent_permissions_agent_id
  ON agent_permissions (agent_id);

CREATE INDEX IF NOT EXISTS idx_agent_permissions_agent_perm
  ON agent_permissions (agent_id, permission);
