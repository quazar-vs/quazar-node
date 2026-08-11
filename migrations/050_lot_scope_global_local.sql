-- Global vs local lots (from SVOD «Доступ»: Г → global, иначе local).
-- Local lots: per-node balances + 9-step export protocol (СБ→СБ→Страж→Страж).

ALTER TABLE service_catalog
    ADD COLUMN IF NOT EXISTS lot_scope TEXT NOT NULL DEFAULT 'global'
        CHECK (lot_scope IN ('global', 'local')),
    ADD COLUMN IF NOT EXISTS access_roles TEXT[] NOT NULL DEFAULT ARRAY['Гражданин']::TEXT[];

CREATE INDEX IF NOT EXISTS idx_service_catalog_lot_scope ON service_catalog (lot_scope);

ALTER TABLE exchange_listings
    ADD COLUMN IF NOT EXISTS lot_scope TEXT NOT NULL DEFAULT 'global'
        CHECK (lot_scope IN ('global', 'local')),
    ADD COLUMN IF NOT EXISTS node_id TEXT;

ALTER TABLE exchange_deals
    ADD COLUMN IF NOT EXISTS lot_scope TEXT NOT NULL DEFAULT 'global'
        CHECK (lot_scope IN ('global', 'local')),
    ADD COLUMN IF NOT EXISTS confirm_mode TEXT NOT NULL DEFAULT 'live_guardian'
        CHECK (confirm_mode IN ('agent_sb', 'live_guardian'));

ALTER TABLE exchange_security_tasks
    ADD COLUMN IF NOT EXISTS confirm_mode TEXT NOT NULL DEFAULT 'live_guardian'
        CHECK (confirm_mode IN ('agent_sb', 'live_guardian')),
    ADD COLUMN IF NOT EXISTS confirmed_by TEXT;

-- Unified per-city stock of local lots (visible to all nodes via shared registry DB).
CREATE TABLE IF NOT EXISTS node_lot_balances (
    node_id TEXT NOT NULL,
    svod_code TEXT NOT NULL,
    qty BIGINT NOT NULL DEFAULT 0 CHECK (qty >= 0),
    reserved BIGINT NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (node_id, svod_code),
    CHECK (reserved <= qty)
);

CREATE INDEX IF NOT EXISTS idx_node_lot_balances_code ON node_lot_balances (svod_code);
CREATE INDEX IF NOT EXISTS idx_node_lot_balances_avail
    ON node_lot_balances (svod_code, (qty - reserved))
    WHERE (qty - reserved) > 0;

-- Inter-city export: 9 steps (open → … → completed).
CREATE TABLE IF NOT EXISTS exchange_lot_transfers (
    id TEXT PRIMARY KEY,
    listing_id TEXT REFERENCES exchange_listings(id),
    deal_id TEXT REFERENCES exchange_deals(id),
    svod_code TEXT NOT NULL,
    from_node_id TEXT NOT NULL,
    to_node_id TEXT NOT NULL,
    qty BIGINT NOT NULL CHECK (qty > 0),
    unit_svod BIGINT NOT NULL DEFAULT 0,
    -- Price always = SVOD; no intercity markup.
    status TEXT NOT NULL,
    -- opened | buyer_sb | owner_sb | reserved | owner_guardian
    -- | transferred | listed_buyer | buyer_receipt | completed | cancelled
    step INT NOT NULL DEFAULT 1,
    opened_by TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

ALTER TABLE exchange_lot_transfers
    ADD COLUMN IF NOT EXISTS unit_svod BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS step INT NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS opened_by TEXT;

CREATE INDEX IF NOT EXISTS idx_exchange_lot_transfers_status ON exchange_lot_transfers (status);
CREATE INDEX IF NOT EXISTS idx_exchange_lot_transfers_nodes
    ON exchange_lot_transfers (from_node_id, to_node_id);
