-- Brings a database created by an older package up to the tables of schema.sql.
--
-- `substreams-sink-sql setup` applies schema.sql, which only creates a table it does not find, so
-- a column or index added there never reaches an existing database. Each one is added here as
-- well.
--
-- Every sink and the pricer apply this file on start, before views.sql, so it has to be
-- idempotent and it has to tolerate a database whose sinks have not created the tables yet.
--
--   psql "$DSN" -f migrate_schema.sql
BEGIN;

-- The same lock executors.sql and views.sql take, so the containers wait for each other instead
-- of racing on the same DDL.
DO $$ BEGIN PERFORM pg_advisory_xact_lock(6045170023); END $$;

ALTER TABLE IF EXISTS trades ADD COLUMN IF NOT EXISTS state_committed BOOLEAN;
ALTER TABLE IF EXISTS trades ADD COLUMN IF NOT EXISTS router_positive_slippage_bps BIGINT;
ALTER TABLE IF EXISTS trades ADD COLUMN IF NOT EXISTS client_positive_slippage_share_bps BIGINT;
ALTER TABLE IF EXISTS trades ADD COLUMN IF NOT EXISTS positive_slippage_cap_bps BIGINT;
ALTER TABLE IF EXISTS trades ADD COLUMN IF NOT EXISTS token_pair_fee BOOLEAN;
ALTER TABLE IF EXISTS fee_config_events ADD COLUMN IF NOT EXISTS token0 TEXT;
ALTER TABLE IF EXISTS fee_config_events ADD COLUMN IF NOT EXISTS token1 TEXT;
ALTER TABLE IF EXISTS router_call_errors ADD COLUMN IF NOT EXISTS state_committed BOOLEAN;

-- Looking up the trades of one transaction by hash, which no other index serves. schema.sql does
-- not declare it, so this is where every database, fresh or not, gets it.
DO $$
BEGIN
    IF to_regclass('public.trades') IS NOT NULL THEN
        CREATE INDEX IF NOT EXISTS trades_tx_idx ON trades (chain, tx_hash);
    END IF;
END $$;

COMMIT;
