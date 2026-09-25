-- Supports the token cache balance delta poll, which reads recently traded tokens.
-- CONCURRENTLY: this table is large and on the extractor's hot write path.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_component_balance_default_valid_from
ON component_balance_default (valid_from);
