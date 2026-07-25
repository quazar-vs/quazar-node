-- Remove internal mailbox provisioning columns; keep external_email for SMTP delivery.
DROP INDEX IF EXISTS idx_applications_internal_email;
DROP INDEX IF EXISTS idx_mayor_applications_internal_email;

ALTER TABLE applications
    DROP COLUMN IF EXISTS mail_password,
    DROP COLUMN IF EXISTS mail_created,
    DROP COLUMN IF EXISTS internal_email;

ALTER TABLE mayor_applications
    DROP COLUMN IF EXISTS mail_password,
    DROP COLUMN IF EXISTS mail_created,
    DROP COLUMN IF EXISTS internal_email;
