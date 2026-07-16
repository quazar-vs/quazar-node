-- Two candidacy kinds on citizens: citizenship (→ Citizen) and role (→ exact target_role).

ALTER TABLE citizens
  ADD COLUMN IF NOT EXISTS candidate_type TEXT,
  ADD COLUMN IF NOT EXISTS target_role TEXT;

-- Backfill existing citizenship Candidates (pre-migration apply flow).
UPDATE citizens
SET candidate_type = 'citizenship',
    target_role = COALESCE(NULLIF(target_role, ''), 'Citizen')
WHERE role = 'Candidate'
  AND candidate_type IS NULL;

-- Agents still awaiting Judge approve (human citizenship pipeline).
UPDATE citizens c
SET candidate_type = 'citizenship',
    target_role = 'Citizen'
WHERE c.role = 'Agent'
  AND c.is_digital = FALSE
  AND c.candidate_type IS NULL
  AND EXISTS (
      SELECT 1 FROM citizenship_applications a
      WHERE a.citizen_id = c.id
  );

ALTER TABLE citizens
  DROP CONSTRAINT IF EXISTS citizens_candidate_type_check;
ALTER TABLE citizens
  ADD CONSTRAINT citizens_candidate_type_check
  CHECK (candidate_type IS NULL OR candidate_type IN ('citizenship', 'role'));

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
      AND target_role IN ('Guardian', 'Judge', 'Aiya', 'Mayor')
    )
  );

CREATE INDEX IF NOT EXISTS idx_citizens_candidate_type
  ON citizens(candidate_type)
  WHERE candidate_type IS NOT NULL;
