-- Demote seed citizen "successful" from historical Aiya to Citizen.
-- Founder identity is role-based (Founder), not a special API key.

UPDATE citizens
SET
    role = 'Citizen',
    status = 'active',
    passport_issued = TRUE
WHERE name = 'successful';

UPDATE api_keys
SET role = 'Citizen'
WHERE citizen_id IN (SELECT id FROM citizens WHERE name = 'successful');
