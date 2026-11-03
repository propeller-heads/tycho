-- Supports the token cache delta refresh, which polls for recently modified tokens.
CREATE INDEX IF NOT EXISTS idx_token_modified_ts ON token (modified_ts);
