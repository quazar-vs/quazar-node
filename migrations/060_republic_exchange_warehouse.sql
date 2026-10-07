-- Canonical republican exchange: citizen item inventory + republic warehouse
-- (separate from shop_warehouse = local exchange = mayor inventory).
-- Deal pairing: offer_a (sell to republic) → listing → buy_b → settle offer_a.

CREATE TABLE IF NOT EXISTS republic_exchange_warehouse (
    item_code TEXT PRIMARY KEY REFERENCES shop_items(code),
    quantity BIGINT NOT NULL DEFAULT 0 CHECK (quantity >= 0),
    updated_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS citizen_item_inventory (
    citizen_id TEXT NOT NULL REFERENCES citizens(id) ON DELETE CASCADE,
    item_code TEXT NOT NULL REFERENCES shop_items(code),
    quantity BIGINT NOT NULL DEFAULT 0 CHECK (quantity >= 0),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (citizen_id, item_code)
);

CREATE INDEX IF NOT EXISTS idx_citizen_item_inventory_item
    ON citizen_item_inventory (item_code)
    WHERE quantity > 0;

ALTER TABLE exchange_listings
    ADD COLUMN IF NOT EXISTS offer_deal_id TEXT;

CREATE INDEX IF NOT EXISTS idx_exchange_listings_offer_deal
    ON exchange_listings (offer_deal_id)
    WHERE offer_deal_id IS NOT NULL;

ALTER TABLE exchange_deals
    ADD COLUMN IF NOT EXISTS deal_role TEXT,
    ADD COLUMN IF NOT EXISTS paired_deal_id TEXT;

CREATE INDEX IF NOT EXISTS idx_exchange_deals_paired
    ON exchange_deals (paired_deal_id)
    WHERE paired_deal_id IS NOT NULL;

COMMENT ON TABLE republic_exchange_warehouse IS
    'Склад республиканской биржи (предметы). Не путать с shop_warehouse (локальная биржа/мэр/магазин).';
COMMENT ON TABLE citizen_item_inventory IS
    'Инвентарь гражданина: только предметы Свода предметов (ITEMS.md).';
COMMENT ON COLUMN exchange_listings.offer_deal_id IS
    'Сделка A (offer_a), ожидающая покупателя после приёмки СБ.';
COMMENT ON COLUMN exchange_deals.deal_role IS
    'offer_a | buy_b — канонический двухсделочный цикл.';
