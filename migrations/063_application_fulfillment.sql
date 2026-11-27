-- Application fulfillment tracking (canon: Aiya confirms fulfillment of the
-- citizenship application). The candidate marks the application fulfilled;
-- approve_citizen requires a fulfilled application.

ALTER TABLE citizenship_applications
  ADD COLUMN IF NOT EXISTS fulfilled_at TIMESTAMPTZ;
ALTER TABLE citizenship_applications
  ADD COLUMN IF NOT EXISTS fulfilled_by TEXT;