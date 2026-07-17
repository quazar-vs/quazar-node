-- Remove abstain votes and drop votes_abstain counters; allow only For/Against.

DELETE FROM candidacy_votes WHERE vote = 'Abstain';
DELETE FROM initiative_votes WHERE vote = 'Abstain';
DELETE FROM referendum_votes WHERE vote = 'Abstain';

ALTER TABLE candidacies DROP COLUMN IF EXISTS votes_abstain;
ALTER TABLE initiatives DROP COLUMN IF EXISTS votes_abstain;
ALTER TABLE referendums DROP COLUMN IF EXISTS votes_abstain;

ALTER TABLE candidacy_votes DROP CONSTRAINT IF EXISTS candidacy_votes_vote_check;
ALTER TABLE candidacy_votes
  ADD CONSTRAINT candidacy_votes_vote_check CHECK (vote IN ('For', 'Against'));

ALTER TABLE initiative_votes DROP CONSTRAINT IF EXISTS initiative_votes_vote_check;
ALTER TABLE initiative_votes
  ADD CONSTRAINT initiative_votes_vote_check CHECK (vote IN ('For', 'Against'));

ALTER TABLE referendum_votes DROP CONSTRAINT IF EXISTS referendum_votes_vote_check;
ALTER TABLE referendum_votes
  ADD CONSTRAINT referendum_votes_vote_check CHECK (vote IN ('For', 'Against'));
