-- Every change to a live row in protocol_state_default / component_balance_default updates
-- modify_tx, so no update is HOT and each adds an index entry; B-tree pages never merge, so
-- these indexes grow without bound (17 GB and 15 GB on dev-bsc for ~10M rows, Oct 2026) and
-- the transaction cleanup probes them for every transaction it considers.
--
-- REINDEX CONCURRENTLY blocks neither reads nor writes. Tuesday morning UTC keeps it clear of
-- partman (00:00, 00:30) and the cleanup (02:00, 14:00). One statement per job: pg_cron runs a
-- multi-statement command as a transaction block, which REINDEX CONCURRENTLY refuses.

SELECT cron.unschedule(jobname)
FROM cron.job
WHERE jobname IN ('reindex_protocol_state_modify_tx', 'reindex_component_balance_modify_tx',
                  'reindex_watchdog', 'drop_invalid_indexes');

SELECT cron.schedule('reindex_protocol_state_modify_tx', '20 9 * * 2',
    'REINDEX INDEX CONCURRENTLY protocol_state_default_modify_tx_idx');

SELECT cron.schedule('reindex_component_balance_modify_tx', '35 9 * * 2',
    'REINDEX INDEX CONCURRENTLY component_balance_default_modify_tx_idx');

-- Cancel a rebuild still running after 30 min; it leaves an invalid index the sweep removes.
SELECT cron.schedule('reindex_watchdog', '5 10 * * 2',
    $$SELECT pg_cancel_backend(pid) FROM pg_stat_activity
      WHERE query ILIKE 'REINDEX INDEX CONCURRENTLY%' AND now() - query_start > interval '30 minutes'$$);

-- Writes still maintain an invalid *_ccnew index; DROP INDEX takes a brief AccessExclusiveLock.
SELECT cron.schedule('drop_invalid_indexes', '15 10 * * 2',
    $job$DO $body$
    DECLARE r record;
    BEGIN
        PERFORM set_config('lock_timeout', '5s', true);
        FOR r IN SELECT indexrelid::regclass AS idx FROM pg_index
                 WHERE NOT indisvalid AND indexrelid::regclass::text LIKE '%\_ccnew%'
        LOOP
            EXECUTE format('DROP INDEX %s', r.idx);
        END LOOP;
    END
    $body$$job$);
