-- Mayor role + two-round mayor elections.
ALTER TABLE citizens DROP CONSTRAINT IF EXISTS citizens_role_check;
ALTER TABLE citizens ADD CONSTRAINT citizens_role_check
  CHECK (role IN ('Citizen', 'Judge', 'Guardian', 'Mayor', 'Aiya', 'Founder', 'Agent'));

ALTER TABLE api_keys DROP CONSTRAINT IF EXISTS api_keys_role_check;
ALTER TABLE api_keys ADD CONSTRAINT api_keys_role_check
  CHECK (role IN ('Citizen', 'Judge', 'Guardian', 'Mayor', 'Aiya', 'Founder', 'Agent'));

CREATE TABLE IF NOT EXISTS mayor_elections (
    id TEXT PRIMARY KEY,
    city_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Nominating',
    current_round INTEGER NOT NULL DEFAULT 0,
    winner_id TEXT REFERENCES citizens(id),
    winner_name TEXT,
    created_by TEXT NOT NULL REFERENCES citizens(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    round1_started_at TIMESTAMPTZ,
    round2_started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS mayor_candidates (
    id TEXT PRIMARY KEY,
    election_id TEXT NOT NULL REFERENCES mayor_elections(id) ON DELETE CASCADE,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    citizen_name TEXT NOT NULL,
    nominated_by TEXT NOT NULL REFERENCES citizens(id),
    status TEXT NOT NULL DEFAULT 'Nominated',
    endorsements INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (election_id, citizen_id)
);

CREATE TABLE IF NOT EXISTS mayor_endorsements (
    id TEXT PRIMARY KEY,
    candidate_id TEXT NOT NULL REFERENCES mayor_candidates(id) ON DELETE CASCADE,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (candidate_id, citizen_id)
);

CREATE TABLE IF NOT EXISTS mayor_votes (
    id TEXT PRIMARY KEY,
    election_id TEXT NOT NULL REFERENCES mayor_elections(id) ON DELETE CASCADE,
    round INTEGER NOT NULL,
    voter_id TEXT NOT NULL REFERENCES citizens(id),
    candidate_id TEXT NOT NULL REFERENCES mayor_candidates(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (election_id, round, voter_id)
);

CREATE INDEX IF NOT EXISTS idx_mayor_elections_status ON mayor_elections(status);
CREATE INDEX IF NOT EXISTS idx_mayor_elections_city ON mayor_elections(city_name);
CREATE INDEX IF NOT EXISTS idx_mayor_candidates_election ON mayor_candidates(election_id);
CREATE INDEX IF NOT EXISTS idx_mayor_endorsements_candidate ON mayor_endorsements(candidate_id);
CREATE INDEX IF NOT EXISTS idx_mayor_votes_election_round ON mayor_votes(election_id, round);
