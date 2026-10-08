-- Supports the component index TVL delta load, which reads recently modified TVL rows.
CREATE INDEX IF NOT EXISTS idx_component_tvl_modified_ts ON component_tvl (modified_ts);
