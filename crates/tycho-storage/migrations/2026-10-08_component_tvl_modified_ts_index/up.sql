-- Supports the component index TVL poll, which reads TVL rows by modified_ts.
CREATE INDEX IF NOT EXISTS idx_component_tvl_modified_ts ON component_tvl (modified_ts);
