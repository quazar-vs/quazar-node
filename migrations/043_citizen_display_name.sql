-- Display name for chat / UI (login `name` stays Latin 1–13).
ALTER TABLE citizens
  ADD COLUMN IF NOT EXISTS display_name TEXT;

COMMENT ON COLUMN citizens.display_name IS
  'Optional public display name (may be Cyrillic). Chat uses COALESCE(display_name, name).';

UPDATE citizens
SET display_name = 'Секретарь Квазар'
WHERE name = 'secretary'
  AND (display_name IS NULL OR BTRIM(display_name) = '');
