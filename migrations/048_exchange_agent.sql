-- Агент Биржи (в ядре): потребность гражданина → лот государства (Successful / QZ-1).

ALTER TABLE exchange_listings
    ADD COLUMN IF NOT EXISTS is_state_agent BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_exchange_listings_state_agent
    ON exchange_listings (is_state_agent)
    WHERE is_state_agent = TRUE;

CREATE TABLE IF NOT EXISTS exchange_needs (
    id TEXT PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    svod_code TEXT NOT NULL REFERENCES service_catalog(code),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    status TEXT NOT NULL,
    listing_id TEXT REFERENCES exchange_listings(id),
    unit_svod BIGINT,
    reserve_balance BIGINT,
    reason TEXT,
    created_via TEXT NOT NULL DEFAULT 'api',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_exchange_needs_citizen ON exchange_needs (citizen_id);
CREATE INDEX IF NOT EXISTS idx_exchange_needs_status ON exchange_needs (status);
