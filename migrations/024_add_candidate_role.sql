-- Allow Candidate role on citizens and api_keys (lowest privilege).
ALTER TABLE citizens DROP CONSTRAINT IF EXISTS citizens_role_check;
ALTER TABLE citizens ADD CONSTRAINT citizens_role_check
  CHECK (role IN ('Citizen', 'Judge', 'Guardian', 'Mayor', 'Aiya', 'Founder', 'Agent', 'Candidate'));

ALTER TABLE api_keys DROP CONSTRAINT IF EXISTS api_keys_role_check;
ALTER TABLE api_keys ADD CONSTRAINT api_keys_role_check
  CHECK (role IN ('Citizen', 'Judge', 'Guardian', 'Mayor', 'Aiya', 'Founder', 'Agent', 'Candidate'));
