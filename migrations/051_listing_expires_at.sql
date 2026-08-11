-- Listing shelf life: store expires_at (not shelf_life days). Default 30 days for existing rows.

ALTER TABLE exchange_listings
    ADD COLUMN IF NOT EXISTS expires_at BIGINT;

-- Backfill active listings without expiry: now + 30 days.
UPDATE exchange_listings
SET expires_at = (EXTRACT(EPOCH FROM NOW())::BIGINT + 2592000)
WHERE expires_at IS NULL AND status = 'active';

CREATE INDEX IF NOT EXISTS idx_exchange_listings_expires
    ON exchange_listings (status, expires_at)
    WHERE status = 'active';
