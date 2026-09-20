-- Exchange board: actions (SVOD) + items (ITEMS).
-- Item lots procure into shop_warehouse; shop still sells from node stock only.

-- --- lot_kind + item_code on exchange tables ---

ALTER TABLE exchange_listings
    ADD COLUMN IF NOT EXISTS lot_kind TEXT NOT NULL DEFAULT 'action',
    ADD COLUMN IF NOT EXISTS item_code TEXT;

ALTER TABLE exchange_needs
    ADD COLUMN IF NOT EXISTS lot_kind TEXT NOT NULL DEFAULT 'action',
    ADD COLUMN IF NOT EXISTS item_code TEXT;

ALTER TABLE exchange_requests
    ADD COLUMN IF NOT EXISTS lot_kind TEXT NOT NULL DEFAULT 'action',
    ADD COLUMN IF NOT EXISTS item_code TEXT;

ALTER TABLE exchange_deals
    ADD COLUMN IF NOT EXISTS lot_kind TEXT NOT NULL DEFAULT 'action',
    ADD COLUMN IF NOT EXISTS item_code TEXT;

-- Drop FK so item rows can omit svod_code (item_code → shop_items instead).
ALTER TABLE exchange_listings DROP CONSTRAINT IF EXISTS exchange_listings_svod_code_fkey;
ALTER TABLE exchange_needs DROP CONSTRAINT IF EXISTS exchange_needs_svod_code_fkey;
ALTER TABLE exchange_requests DROP CONSTRAINT IF EXISTS exchange_requests_svod_code_fkey;

ALTER TABLE exchange_listings ALTER COLUMN svod_code DROP NOT NULL;
ALTER TABLE exchange_needs ALTER COLUMN svod_code DROP NOT NULL;
ALTER TABLE exchange_requests ALTER COLUMN svod_code DROP NOT NULL;
ALTER TABLE exchange_deals ALTER COLUMN svod_code DROP NOT NULL;

-- Soft FK: item_code must exist in shop_items when set.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_listings_item_code_fkey'
    ) THEN
        ALTER TABLE exchange_listings
            ADD CONSTRAINT exchange_listings_item_code_fkey
            FOREIGN KEY (item_code) REFERENCES shop_items(code);
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_needs_item_code_fkey'
    ) THEN
        ALTER TABLE exchange_needs
            ADD CONSTRAINT exchange_needs_item_code_fkey
            FOREIGN KEY (item_code) REFERENCES shop_items(code);
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_requests_item_code_fkey'
    ) THEN
        ALTER TABLE exchange_requests
            ADD CONSTRAINT exchange_requests_item_code_fkey
            FOREIGN KEY (item_code) REFERENCES shop_items(code);
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_deals_item_code_fkey'
    ) THEN
        ALTER TABLE exchange_deals
            ADD CONSTRAINT exchange_deals_item_code_fkey
            FOREIGN KEY (item_code) REFERENCES shop_items(code);
    END IF;
END $$;

-- kind ↔ code consistency
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_listings_lot_kind_check'
    ) THEN
        ALTER TABLE exchange_listings
            ADD CONSTRAINT exchange_listings_lot_kind_check
            CHECK (lot_kind IN ('action', 'item'));
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_listings_kind_codes_check'
    ) THEN
        ALTER TABLE exchange_listings
            ADD CONSTRAINT exchange_listings_kind_codes_check
            CHECK (
                (lot_kind = 'action' AND svod_code IS NOT NULL AND item_code IS NULL)
                OR (lot_kind = 'item' AND item_code IS NOT NULL AND svod_code IS NULL)
            );
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_needs_lot_kind_check'
    ) THEN
        ALTER TABLE exchange_needs
            ADD CONSTRAINT exchange_needs_lot_kind_check
            CHECK (lot_kind IN ('action', 'item'));
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'exchange_needs_kind_codes_check'
    ) THEN
        ALTER TABLE exchange_needs
            ADD CONSTRAINT exchange_needs_kind_codes_check
            CHECK (
                (lot_kind = 'action' AND svod_code IS NOT NULL AND item_code IS NULL)
                OR (lot_kind = 'item' AND item_code IS NOT NULL AND svod_code IS NULL)
            );
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_exchange_listings_lot_kind ON exchange_listings (lot_kind);
CREATE INDEX IF NOT EXISTS idx_exchange_listings_item_code ON exchange_listings (item_code)
    WHERE item_code IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_exchange_needs_lot_kind ON exchange_needs (lot_kind);
CREATE INDEX IF NOT EXISTS idx_exchange_needs_item_code ON exchange_needs (item_code)
    WHERE item_code IS NOT NULL;

-- Cumulative units procured via exchange → warehouse.
CREATE TABLE IF NOT EXISTS exchange_item_created (
    item_code TEXT PRIMARY KEY REFERENCES shop_items(code),
    created_qty BIGINT NOT NULL DEFAULT 0 CHECK (created_qty >= 0),
    updated_at BIGINT NOT NULL
);

COMMENT ON TABLE exchange_item_created IS
    'Lifetime units of shop items credited to warehouses via exchange fulfillment';

-- Action counters: ordered (open needs) / completed (finished deals).
CREATE OR REPLACE VIEW exchange_action_board AS
SELECT
    sc.code AS svod_code,
    sc.name AS title,
    sc.base_price AS unit_svod,
    sc.work_kind,
    COALESCE(sc.lot_scope, 'global') AS lot_scope,
    COALESCE(o.ordered_qty, 0)::bigint AS ordered_qty,
    COALESCE(c.completed_qty, 0)::bigint AS completed_qty
FROM service_catalog sc
LEFT JOIN (
    SELECT svod_code, SUM(quantity)::bigint AS ordered_qty
    FROM exchange_needs
    WHERE lot_kind = 'action'
      AND status IN ('pending', 'listed')
      AND svod_code IS NOT NULL
    GROUP BY svod_code
) o ON o.svod_code = sc.code
LEFT JOIN (
    SELECT svod_code, SUM(qty)::bigint AS completed_qty
    FROM exchange_deals
    WHERE lot_kind = 'action'
      AND status = 'completed'
      AND svod_code IS NOT NULL
    GROUP BY svod_code
) c ON c.svod_code = sc.code
WHERE sc.is_active = TRUE;

-- Item counters: created (via exchange) / total (all nodes) / on_node filled by API.
CREATE OR REPLACE VIEW exchange_item_board AS
SELECT
    i.code AS item_code,
    i.title,
    i.category,
    i.unit,
    i.price_kvazi AS unit_svod,
    COALESCE(c.created_qty, 0)::bigint AS created_qty,
    COALESCE(t.total_qty, 0)::bigint AS total_qty
FROM shop_items i
LEFT JOIN exchange_item_created c ON c.item_code = i.code
LEFT JOIN (
    SELECT item_id, SUM(quantity)::bigint AS total_qty
    FROM shop_warehouse
    GROUP BY item_id
) t ON t.item_id = i.id
WHERE i.is_active = TRUE;

COMMENT ON VIEW exchange_action_board IS 'Exchange board rows for Svod of Actions';
COMMENT ON VIEW exchange_item_board IS 'Exchange board rows for Svod of Items (on_node joined in API)';
