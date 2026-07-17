-- Allow Citizen as target_role for role candidacies (nominate → vote → appoint).

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
      AND target_role IN ('Citizen', 'Guardian', 'Judge', 'Aiya', 'Mayor')
    )
  );
