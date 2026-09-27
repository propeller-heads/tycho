-- Legacy rows keep their existing admission behavior. Pending is distinct from token quality.
ALTER TABLE token ADD COLUMN metadata_pending BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX token_pending_metadata_idx ON token (id) WHERE metadata_pending;
