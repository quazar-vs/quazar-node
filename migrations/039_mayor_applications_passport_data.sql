-- Street mayor applications: store passport data like citizenship applications.

ALTER TABLE mayor_applications
    ADD COLUMN IF NOT EXISTS passport_data TEXT;
