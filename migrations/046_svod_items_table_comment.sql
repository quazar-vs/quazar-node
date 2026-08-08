-- Clarify: service_catalog is storage for Государственный Свод Вознаграждений items (exchange positions).
-- Public API/product name is never "catalog" — only Свод parts.*.items / rates.

COMMENT ON TABLE service_catalog IS
  'Позиции Государственного Свода Вознаграждений (биржевые items / parts.*.items).';

COMMENT ON TABLE service_categories IS
  'Группы позиций Свода (внутренние категории), не отдельный реестр экономики.';
