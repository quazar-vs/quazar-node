-- Rename legacy seed citizen id `seed-master-aiya-001` → `seed-successful-001`.
-- Update FK child rows first, then the citizens primary key.

DO $$
DECLARE
    old_id TEXT := 'seed-master-aiya-001';
    new_id TEXT := 'seed-successful-001';
BEGIN
    IF NOT EXISTS (SELECT 1 FROM citizens WHERE id = old_id) THEN
        RETURN;
    END IF;
    IF EXISTS (SELECT 1 FROM citizens WHERE id = new_id) THEN
        RETURN;
    END IF;

    IF to_regclass('public.api_keys') IS NOT NULL THEN
        UPDATE api_keys SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.passports') IS NOT NULL THEN
        UPDATE passports SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.citizen_credentials') IS NOT NULL THEN
        UPDATE citizen_credentials SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.candidacies') IS NOT NULL THEN
        UPDATE candidacies SET citizen_id = new_id WHERE citizen_id = old_id;
        UPDATE candidacies SET nominator_id = new_id WHERE nominator_id = old_id;
    END IF;
    IF to_regclass('public.candidacy_votes') IS NOT NULL THEN
        UPDATE candidacy_votes SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.chat_messages') IS NOT NULL THEN
        UPDATE chat_messages SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.chat_sessions') IS NOT NULL THEN
        UPDATE chat_sessions SET user_id = new_id WHERE user_id = old_id;
    END IF;
    IF to_regclass('public.initiatives') IS NOT NULL THEN
        UPDATE initiatives SET proposer_id = new_id WHERE proposer_id = old_id;
    END IF;
    IF to_regclass('public.initiative_votes') IS NOT NULL THEN
        UPDATE initiative_votes SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.referendums') IS NOT NULL THEN
        UPDATE referendums SET announcer_id = new_id WHERE announcer_id = old_id;
    END IF;
    IF to_regclass('public.referendum_votes') IS NOT NULL THEN
        UPDATE referendum_votes SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.agent_permissions') IS NOT NULL THEN
        UPDATE agent_permissions SET agent_id = new_id WHERE agent_id = old_id;
    END IF;
    IF to_regclass('public.citizenship_applications') IS NOT NULL THEN
        UPDATE citizenship_applications SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.applications') IS NOT NULL THEN
        UPDATE applications SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.mayor_elections') IS NOT NULL THEN
        UPDATE mayor_elections SET winner_id = new_id WHERE winner_id = old_id;
        UPDATE mayor_elections SET created_by = new_id WHERE created_by = old_id;
    END IF;
    IF to_regclass('public.mayor_candidates') IS NOT NULL THEN
        UPDATE mayor_candidates SET citizen_id = new_id WHERE citizen_id = old_id;
        UPDATE mayor_candidates SET nominated_by = new_id WHERE nominated_by = old_id;
    END IF;
    IF to_regclass('public.mayor_endorsements') IS NOT NULL THEN
        UPDATE mayor_endorsements SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.mayor_votes') IS NOT NULL THEN
        UPDATE mayor_votes SET voter_id = new_id WHERE voter_id = old_id;
    END IF;
    IF to_regclass('public.citizen_status') IS NOT NULL THEN
        UPDATE citizen_status SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;
    IF to_regclass('public.vote_choices') IS NOT NULL THEN
        UPDATE vote_choices SET citizen_id = new_id WHERE citizen_id = old_id;
    END IF;

    UPDATE citizens SET id = new_id WHERE id = old_id;
END $$;
