-- Presence-before-economy: citizens.status may be 'online' after network connect,
-- then 'active' after VPN = role acceptance.

ALTER TABLE citizens DROP CONSTRAINT IF EXISTS citizens_status_check;
ALTER TABLE citizens ADD CONSTRAINT citizens_status_check
  CHECK (status IN ('applicant', 'pending', 'online', 'active', 'suspended', 'revoked'));
