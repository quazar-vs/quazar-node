-- Councils (граждан, безопасности, экономический) and the Court are abolished:
-- justice and security are Aiya's functions; codes are adopted by citizen vote.
-- Citizenship: application fulfillment confirmed by Aiya → Citizen;
-- the passport follows identity verification by a verified citizen.

-- Identity verification flag (passport prerequisite).
ALTER TABLE citizens
  ADD COLUMN IF NOT EXISTS identity_verified BOOLEAN NOT NULL DEFAULT FALSE;

-- Existing passport holders are grandfathered as identity-verified.
UPDATE citizens SET identity_verified = TRUE WHERE passport_issued = TRUE;

-- Abolished statuses: Guardian (Страж) and Judge (Судья) → Citizen.
UPDATE api_keys SET role = 'Citizen' WHERE role IN ('Guardian', 'Judge');
UPDATE citizens SET role = 'Citizen' WHERE role IN ('Guardian', 'Judge');

-- Human Agent-stage citizens (old SB-verify pipeline) re-enter the new flow:
-- Candidate awaiting Aiya's confirmation of the application fulfillment.
-- Their old Agent keys are disabled; a new key is issued with the passport.
UPDATE api_keys SET is_active = FALSE
WHERE citizen_id IN (
    SELECT id FROM citizens
    WHERE role = 'Agent' AND is_digital = FALSE
      AND (candidate_type = 'citizenship'
           OR EXISTS (SELECT 1 FROM citizenship_applications a WHERE a.citizen_id = citizens.id))
);
UPDATE citizens
SET role = 'Candidate',
    status = 'pending',
    candidate_type = 'citizenship',
    target_role = 'Citizen'
WHERE role = 'Agent' AND is_digital = FALSE
  AND (candidate_type = 'citizenship'
       OR EXISTS (SELECT 1 FROM citizenship_applications a WHERE a.citizen_id = citizens.id));

-- Void pending Guardian/Judge candidacies: the statuses no longer exist.
UPDATE candidacies SET status = 'Rejected'
WHERE target_role IN ('Guardian', 'Judge') AND status IN ('Active', 'Approved');
UPDATE citizens SET candidate_type = NULL, target_role = NULL
WHERE candidate_type = 'role' AND target_role IN ('Guardian', 'Judge');

-- Only Citizen and Mayor remain as role-candidacy targets.
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
      AND target_role IN ('Citizen', 'Mayor')
    )
  );