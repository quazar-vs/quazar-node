-- Aiya is the quazar entity itself (the algonism / quazar / republic), not an
-- elected office. Former separate agents (secretary / sb / exchange / lawyer)
-- are functions of the single system Aiya entity.

-- Free the canonical name if a legacy citizen holds it.
UPDATE citizens SET name = name || ' (гражданин)'
WHERE name = 'Айя' AND id <> 'aiya';

-- The single Aiya system citizen; the node acts as this entity.
INSERT INTO citizens (id, name, public_key, status, role, created_at, passport_issued)
VALUES ('aiya', 'Айя', 'aiya-system-entity', 'active', 'Aiya', EXTRACT(EPOCH FROM NOW())::BIGINT, TRUE)
ON CONFLICT (id) DO UPDATE SET role = 'Aiya', status = 'active', passport_issued = TRUE;

-- All former agent bindings (state and city) belong to Aiya.
UPDATE algonism_agents SET citizen_id = 'aiya' WHERE citizen_id <> 'aiya';

-- Legacy human Aiya holders: Aiya is no longer a human role.
UPDATE api_keys SET role = 'Citizen'
WHERE role = 'Aiya' AND citizen_id <> 'aiya';
UPDATE citizens SET role = 'Citizen'
WHERE role = 'Aiya' AND id <> 'aiya';

-- Pending Aiya candidacies are void: the role is not electable.
UPDATE candidacies SET status = 'Rejected'
WHERE target_role = 'Aiya' AND status IN ('Active', 'Approved');
UPDATE citizens SET candidate_type = NULL, target_role = NULL
WHERE candidate_type = 'role' AND target_role = 'Aiya';

-- Aiya removed from role-candidacy targets.
ALTER TABLE citizens
  DROP CONSTRAINT IF EXISTS citizens_candidate_fields_check;
ALTER TABLE citizens
  ADD CONSTRAINT citizens_candidate_fields_check
  CHECK (
    (candidate_type IS NULL AND (target_role IS NULL OR target_role = ''))
    OR (
      candidate_type = 'citizenship'
      AND (target_role IS NULL OR target_role = 'Citizen')
    )
    OR (
      candidate_type = 'role'
      AND target_role IS NOT NULL
      AND target_role <> ''
      AND target_role IN ('Citizen', 'Guardian', 'Judge', 'Mayor')
    )
  );