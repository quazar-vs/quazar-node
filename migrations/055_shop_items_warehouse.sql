-- Phase 3: Republic Svod of Items + node warehouse + shop purchases.
-- Separate from exchange/service_catalog (Svod of Actions) and REFLEXES (Svod of Reflexes).

CREATE TABLE IF NOT EXISTS shop_items (
    id UUID PRIMARY KEY,
    code TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    category TEXT NOT NULL CHECK (category IN ('товары', 'услуги')),
    unit TEXT NOT NULL DEFAULT '1 шт',
    description TEXT NOT NULL DEFAULT '',
    -- Interim republic-internal price in KVAZI (ЭК shop fiat = future rails).
    price_kvazi BIGINT NOT NULL CHECK (price_kvazi >= 0),
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_shop_items_category ON shop_items (category);
CREATE INDEX IF NOT EXISTS idx_shop_items_active ON shop_items (is_active);

CREATE TABLE IF NOT EXISTS shop_warehouse (
    id UUID PRIMARY KEY,
    node_id TEXT NOT NULL,
    item_id UUID NOT NULL REFERENCES shop_items(id),
    quantity BIGINT NOT NULL DEFAULT 0 CHECK (quantity >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (node_id, item_id)
);

CREATE INDEX IF NOT EXISTS idx_shop_warehouse_node ON shop_warehouse (node_id);

CREATE TABLE IF NOT EXISTS shop_purchases (
    id UUID PRIMARY KEY,
    citizen_id TEXT NOT NULL REFERENCES citizens(id),
    item_id UUID NOT NULL REFERENCES shop_items(id),
    node_id TEXT NOT NULL,
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    total_price BIGINT NOT NULL CHECK (total_price >= 0),
    commission BIGINT NOT NULL CHECK (commission >= 0),
    mayor_share BIGINT NOT NULL CHECK (mayor_share >= 0),
    mayor_citizen_id TEXT,
    currency TEXT NOT NULL DEFAULT 'КВАЗИ',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_shop_purchases_citizen ON shop_purchases (citizen_id);
CREATE INDEX IF NOT EXISTS idx_shop_purchases_node ON shop_purchases (node_id);
CREATE INDEX IF NOT EXISTS idx_shop_purchases_created ON shop_purchases (created_at DESC);

COMMENT ON TABLE shop_items IS 'Republic Svod of Items (ITEMS.md) — catalog for node warehouse/shop';
COMMENT ON TABLE shop_warehouse IS 'Per-node stock of shop items; filled via mayor stock (exchange = procurement channel)';
COMMENT ON TABLE shop_purchases IS 'Shop purchases on a node; interim KVAZI 10% state / 90% mayor';

-- Seed demo items for the Items svod (idempotent by code).
INSERT INTO shop_items (id, code, title, category, unit, description, price_kvazi, is_active)
VALUES
    ('a1111111-1111-4111-8111-111111111101'::uuid, 'water_bottle', 'Бутылка воды', 'товары', '1 шт',
     'Питьевая вода со склада узла', 5, TRUE),
    ('a1111111-1111-4111-8111-111111111102'::uuid, 'city_map', 'Карта города', 'товары', '1 шт',
     'Печатная карта узла (города)', 10, TRUE),
    ('a1111111-1111-4111-8111-111111111103'::uuid, 'node_tour', 'Экскурсия по узлу', 'услуги', '1 час',
     'Локальная услуга сопровождения по инфраструктуре города', 50, TRUE)
ON CONFLICT (code) DO NOTHING;

-- Initial stock on this republic node (empty until mayor stocks — seed small demo qty).
INSERT INTO shop_warehouse (id, node_id, item_id, quantity, updated_at)
SELECT gen_random_uuid(), 'QZ-NODE-1', i.id, 20, NOW()
FROM shop_items i
WHERE i.code IN ('water_bottle', 'city_map', 'node_tour')
ON CONFLICT (node_id, item_id) DO NOTHING;
