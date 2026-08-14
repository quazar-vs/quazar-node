-- Allow Algonism Lawyer agent kind (RX_LAW_* / DNA v2.3).
ALTER TABLE algonism_agents
    DROP CONSTRAINT IF EXISTS algonism_agents_agent_kind_check;

ALTER TABLE algonism_agents
    ADD CONSTRAINT algonism_agents_agent_kind_check
    CHECK (agent_kind IN ('secretary', 'sb', 'exchange', 'lawyer'));
