-- Moves an existing database from the ETH-denominated pricing columns to the USD ones.
--
-- Runs on every pricer start, so it has to be idempotent and it has to tolerate a database whose
-- sinks have not created `trades` yet.
--
-- The old values are dropped rather than converted: each held the trade valued in the chain's
-- native token while the column claimed ETH, which was wrong for bsc (BNB) and polygon (POL), and
-- they covered 0.2% of rows.
--
--   psql "$DSN" -f pricing/migrate_usd.sql
DO $$
BEGIN
    IF to_regclass('public.trades') IS NULL THEN
        RAISE NOTICE 'trades table does not exist yet, skipping';
        RETURN;
    END IF;

    ALTER TABLE trades ADD COLUMN IF NOT EXISTS price_in_usd  DOUBLE PRECISION;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS price_out_usd DOUBLE PRECISION;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS volume_usd    DOUBLE PRECISION;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS price_source  TEXT;
    ALTER TABLE trades ADD COLUMN IF NOT EXISTS native_usd    DOUBLE PRECISION;

    ALTER TABLE trades DROP COLUMN IF EXISTS price_in_eth;
    ALTER TABLE trades DROP COLUMN IF EXISTS price_out_eth;
    ALTER TABLE trades DROP COLUMN IF EXISTS volume_eth;

    -- Offer rows priced by the old logic to the new one. A row the new logic priced carries a
    -- price_source, so it is never reset and the pass does not repeat.
    UPDATE trades SET priced_at = NULL WHERE priced_at IS NOT NULL AND price_source IS NULL;
END $$;
