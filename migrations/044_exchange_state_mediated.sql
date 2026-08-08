-- State-mediated exchange: citizen ↔ treasury via Security (Guardian).
-- Legacy P2P offers/orders kept for history; new tables drive the API.

CREATE TABLE IF NOT EXISTS exchange_inventory (
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    svod_code TEXT NOT NULL REFERENCES service_catalog(code),
    quantity BIGINT NOT NULL DEFAULT 0 CHECK (quantity >= 0),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (citizen_id, svod_code)
);

CREATE TABLE IF NOT EXISTS exchange_requests (
    id TEXT PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    side TEXT NOT NULL CHECK (side IN ('sell', 'buy')),
    svod_code TEXT NOT NULL REFERENCES service_catalog(code),
    qty_total BIGINT NOT NULL CHECK (qty_total > 0),
    qty_remaining BIGINT NOT NULL CHECK (qty_remaining >= 0),
    unit_svod BIGINT NOT NULL CHECK (unit_svod > 0),
    status TEXT NOT NULL,
    listing_id TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_exchange_requests_citizen ON exchange_requests(citizen_id);
CREATE INDEX IF NOT EXISTS idx_exchange_requests_status ON exchange_requests(status);

CREATE TABLE IF NOT EXISTS exchange_listings (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL REFERENCES exchange_requests(id),
    seller_id TEXT NOT NULL REFERENCES citizens(id),
    svod_code TEXT NOT NULL REFERENCES service_catalog(code),
    qty_total BIGINT NOT NULL CHECK (qty_total > 0),
    qty_remaining BIGINT NOT NULL CHECK (qty_remaining >= 0),
    unit_svod BIGINT NOT NULL CHECK (unit_svod > 0),
    unit_buyer_price BIGINT NOT NULL CHECK (unit_buyer_price > 0),
    unit_seller_price BIGINT NOT NULL CHECK (unit_seller_price > 0),
    status TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_exchange_listings_status ON exchange_listings(status);

CREATE TABLE IF NOT EXISTS exchange_deals (
    id TEXT PRIMARY KEY,
    listing_id TEXT NOT NULL REFERENCES exchange_listings(id),
    request_id TEXT NOT NULL REFERENCES exchange_requests(id),
    buyer_id TEXT NOT NULL REFERENCES citizens(id),
    seller_id TEXT NOT NULL REFERENCES citizens(id),
    svod_code TEXT NOT NULL,
    qty BIGINT NOT NULL CHECK (qty > 0),
    unit_svod BIGINT NOT NULL,
    unit_buyer_price BIGINT NOT NULL,
    unit_seller_price BIGINT NOT NULL,
    escrow_amount BIGINT NOT NULL,
    status TEXT NOT NULL,
    security_task_id TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_exchange_deals_buyer ON exchange_deals(buyer_id);
CREATE INDEX IF NOT EXISTS idx_exchange_deals_status ON exchange_deals(status);

CREATE TABLE IF NOT EXISTS exchange_security_tasks (
    id TEXT PRIMARY KEY,
    deal_id TEXT NOT NULL REFERENCES exchange_deals(id),
    from_citizen TEXT NOT NULL,
    to_citizen TEXT NOT NULL,
    svod_code TEXT NOT NULL,
    qty BIGINT NOT NULL CHECK (qty > 0),
    status TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_exchange_security_tasks_status ON exchange_security_tasks(status);

-- Treasury balance row (no FK to citizens).
INSERT INTO balances (citizen_id, amount)
VALUES ('quazar', 0)
ON CONFLICT (citizen_id) DO NOTHING;

-- Drop legacy P2P marketplace tables (replaced by state-mediated exchange).
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS offers;
