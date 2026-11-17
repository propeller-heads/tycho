-- Assertions for migrate_schema.sql: every column and index it adds is there afterwards.
--
-- Run through scripts/test_views.sh, which first drops them from a fresh schema.sql to stand in for
-- a database an older package created, and then applies the migration.
DO $$
DECLARE
    missing TEXT;
BEGIN
    SELECT string_agg(format('%s.%s', expected.tbl, expected.col), ', ')
    INTO missing
    FROM (VALUES
        ('trades', 'state_committed'),
        ('trades', 'router_positive_slippage_bps'),
        ('trades', 'client_positive_slippage_share_bps'),
        ('trades', 'positive_slippage_cap_bps'),
        ('trades', 'token_pair_fee'),
        ('fee_config_events', 'token0'),
        ('fee_config_events', 'token1'),
        ('router_call_errors', 'state_committed')
    ) AS expected (tbl, col)
    WHERE NOT EXISTS (
        SELECT 1 FROM information_schema.columns c
        WHERE c.table_schema = 'public' AND c.table_name = expected.tbl
          AND c.column_name = expected.col
    );
    IF missing IS NOT NULL THEN
        RAISE EXCEPTION 'migrate_schema.sql did not add: %', missing;
    END IF;

    IF to_regclass('public.trades_tx_idx') IS NULL THEN
        RAISE EXCEPTION 'migrate_schema.sql did not create trades_tx_idx';
    END IF;
END $$;
