-- Clarify: service_catalog is storage for Республиканский Свод Вознаграждений items (exchange positions).
-- Public API/product name is never "catalog" — only Свод parts.*.items / rates.

COMMENT ON TABLE service_catalog IS
  'Позиции Республиканского Свода Вознаграждений (биржевые items / parts.*.items).';

COMMENT ON TABLE service_categories IS
  'Группы позиций Свода (внутренние категории), не отдельный реестр экономики.';
