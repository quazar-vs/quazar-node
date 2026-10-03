-- Shop contour: external payment only (no KVAZI). Warehouse debit for shop-app.

ALTER TABLE shop_purchases
    ALTER COLUMN citizen_id DROP NOT NULL;

ALTER TABLE shop_purchases
    ALTER COLUMN currency SET DEFAULT 'EXTERNAL';

ALTER TABLE shop_purchases
    ADD COLUMN IF NOT EXISTS buyer_external TEXT;

ALTER TABLE shop_purchases
    ADD COLUMN IF NOT EXISTS external_payment_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_shop_purchases_external_payment
    ON shop_purchases (external_payment_id)
    WHERE external_payment_id IS NOT NULL AND btrim(external_payment_id) <> '';

COMMENT ON TABLE shop_purchases IS
    'Shop utilizations after external/fiat payment; no KVAZI. Debit via /api/shop/warehouse/debit';

COMMENT ON COLUMN shop_items.price_kvazi IS
    'Catalog reference only; shop payment is external/fiat (not settled in KVAZI)';
