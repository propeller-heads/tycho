-- Weekly online rebuild of the modify_tx indexes on the live-row ("default") partitions.
--
-- Every state or balance change is an UPDATE of a live row in protocol_state_default or
-- component_balance_default, and modify_tx changes with it, so none of those updates are
-- HOT: each one adds a fresh entry to the modify_tx index. Vacuum frees the old entries but a
-- B-tree page is only recycled once it is completely empty and pages never merge, so after
-- billions of updates the indexes are sparse shells (measured 2026-10-09: 17 GB and 15 GB
-- on dev-bsc for ~10M live rows each, ~99% empty; 6 GB and 2.8 GB on dev-base). Postgres
-- never shrinks them on its own.
--
-- These two indexes are what cleanup_orphaned_transactions probes for every transaction it
-- considers (NOT EXISTS ... WHERE modify_tx = t.id), so their bloat is paid twice a day as a
-- 45-minute disk-read storm (5-7k IOPS on dev-bsc, lifetime ~115 TB read from disk).
--
-- REINDEX INDEX CONCURRENTLY blocks neither reads nor writes (ShareUpdateExclusiveLock),
-- cancels nothing and finishes in minutes for ~10M rows. Its only hazard is overlapping DDL
-- that needs a strong lock on the same partition: a queued AccessExclusiveLock request from
-- partman would make every new query on the partition queue behind it for the rest of the
-- rebuild. The schedule therefore sits on Tuesday morning UTC, far from partman (00:00,
-- 00:30) and the transaction cleanup (02:00, 14:00), when people are around, and a watchdog
-- cancels any rebuild still running after 30 minutes.
--
-- One statement per job: pg_cron sends each command as a single simple query, and a
-- multi-statement command runs as an implicit transaction block, which REINDEX CONCURRENTLY
-- refuses.
--
-- The partition indexes are auto-named by Postgres when the parent (partitioned) index was
-- created, so they are resolved here through pg_inherits instead of being hard-coded; a
-- database where either is missing fails this migration instead of a cron job later.

CREATE OR REPLACE FUNCTION live_partition_index(p_parent_index text, p_partition text)
RETURNS text
LANGUAGE sql
STABLE
AS $$
    SELECT c.relname::text
    FROM pg_inherits i
    JOIN pg_class c ON c.oid = i.inhrelid
    JOIN pg_class p ON p.oid = i.inhparent
    JOIN pg_index x ON x.indexrelid = c.oid
    WHERE p.relname = p_parent_index
      AND x.indrelid = p_partition::regclass;
$$;

COMMENT ON FUNCTION live_partition_index(text, text) IS
    'Name of the partition-level index that p_partition holds for the partitioned index p_parent_index.';

SELECT cron.unschedule(jobname)
FROM cron.job
WHERE jobname IN (
    'reindex_protocol_state_modify_tx',
    'reindex_component_balance_modify_tx',
    'reindex_watchdog',
    'drop_invalid_indexes'
);

DO $do$
DECLARE
    v_protocol_state_idx text := live_partition_index('idx_protocol_state_modify_tx', 'protocol_state_default');
    v_component_balance_idx text := live_partition_index('idx_component_balance_modify_tx', 'component_balance_default');
BEGIN
    IF v_protocol_state_idx IS NULL THEN
        RAISE EXCEPTION 'protocol_state_default has no modify_tx index under idx_protocol_state_modify_tx';
    END IF;
    IF v_component_balance_idx IS NULL THEN
        RAISE EXCEPTION 'component_balance_default has no modify_tx index under idx_component_balance_modify_tx';
    END IF;

    PERFORM cron.schedule(
        'reindex_protocol_state_modify_tx',
        '20 9 * * 2',
        format('REINDEX INDEX CONCURRENTLY %I', v_protocol_state_idx)
    );
    PERFORM cron.schedule(
        'reindex_component_balance_modify_tx',
        '35 9 * * 2',
        format('REINDEX INDEX CONCURRENTLY %I', v_component_balance_idx)
    );
END
$do$;

-- A rebuild still running 30 minutes after the last slot is cancelled. Cancelling REINDEX
-- CONCURRENTLY is safe; it leaves an invalid index that the job below removes.
SELECT cron.schedule(
    'reindex_watchdog',
    '5 10 * * 2',
    $$SELECT pg_cancel_backend(pid) FROM pg_stat_activity
      WHERE query ILIKE 'REINDEX INDEX CONCURRENTLY%' AND now() - query_start > interval '30 minutes'$$
);

-- An interrupted REINDEX CONCURRENTLY leaves an invalid *_ccnew index. Queries ignore it but
-- every write still maintains it, so drop it. Plain DROP INDEX takes a brief
-- AccessExclusiveLock on the table; lock_timeout keeps it from queueing behind anything.
SELECT cron.schedule(
    'drop_invalid_indexes',
    '15 10 * * 2',
    $job$DO $body$
      DECLARE r record;
      BEGIN
          PERFORM set_config('lock_timeout', '5s', true);
          FOR r IN
              SELECT n.nspname, c.relname
              FROM pg_index i
              JOIN pg_class c ON c.oid = i.indexrelid
              JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE NOT i.indisvalid AND c.relname LIKE '%\_ccnew%'
          LOOP
              EXECUTE format('DROP INDEX %I.%I', r.nspname, r.relname);
              RAISE NOTICE 'dropped invalid index %.%', r.nspname, r.relname;
          END LOOP;
      END
      $body$$job$
);
