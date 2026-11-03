-- Decouple partition retention from partition creation
--
-- pg_partman's run_maintenance() creates the upcoming partitions and drops the expired ones
-- inside one transaction per parent table. By the time it issues the retention DROP TABLE it
-- already holds locks on the default partition from the earlier maintenance steps, and the
-- DROP needs an AccessExclusiveLock on the parent. Any concurrent query that acquired an
-- AccessShareLock on the parent in between and then waits on the default partition closes a
-- lock cycle; Postgres resolves it by killing the maintenance run. The rollback also discards
-- the partitions created earlier in the transaction, so a lost retention race silently stops
-- partition premake. When the premade partitions run out, inserts of closed row versions land
-- in the default partition and collide with its (component, token) unique index, crashing the
-- writer.
--
-- Retention is therefore removed from part_config, making the nightly run_maintenance()
-- creation-only: it can no longer be rolled back by a retention failure. Expired partitions
-- are dropped by drop_expired_partitions() below, scheduled separately. Each attempt runs in
-- its own transaction that holds no prior locks when it requests the parent lock, so it can
-- queue behind running queries but cannot deadlock. A short lock_timeout bounds how long the
-- queued request may stall other queries on the parent (lock requests behind a waiting
-- AccessExclusiveLock cannot be granted until it is); on failure the attempt rolls back and
-- is retried after a pause. A run that exhausts its attempts raises at the end, so the
-- failure is visible in cron.job_run_details, and the next scheduled run picks up the
-- surviving partitions. The worst outcome of a lost race is now an expired partition living
-- one day longer, instead of a frozen premake runway.

UPDATE partman.part_config
SET retention = NULL
WHERE parent_table IN ('public.component_balance', 'public.contract_storage', 'public.protocol_state');

-- Single source of truth for the retention horizon, replacing the part_config values cleared
-- above (putting it back into part_config.retention would put the drop back inside
-- run_maintenance). Read on every run by drop_expired_partitions() and by the
-- cleanup_orphaned_transactions cron job (rescheduled below), so an UPDATE of this row takes
-- effect at their next runs with nothing to redeploy. Make permanent changes in a migration
-- too, so freshly created databases match.
CREATE TABLE partition_retention_config (
    id boolean PRIMARY KEY DEFAULT TRUE CHECK (id),
    retention interval NOT NULL DEFAULT '1 month',
    updated_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO partition_retention_config DEFAULT VALUES;

-- Reader for contexts that cannot query the table directly: CALL arguments (as in the cron
-- commands below) accept function calls but not subqueries. Falls back to 1 month if the
-- config row is ever missing.
CREATE OR REPLACE FUNCTION partition_retention()
RETURNS interval
LANGUAGE sql STABLE
AS $$
    SELECT coalesce(min(retention), interval '1 month') FROM partition_retention_config
$$;

-- p_retention NULL (the default) resolves to partition_retention() at run time, so scheduled
-- runs follow the config table. To change which tables are swept, update the array.
CREATE OR REPLACE PROCEDURE drop_expired_partitions(
    p_retention interval DEFAULT NULL,
    p_lock_timeout text DEFAULT '2s',
    p_max_attempts integer DEFAULT 5
)
LANGUAGE plpgsql
AS $$
DECLARE
    v_retention interval := coalesce(p_retention, partition_retention());
    v_parent text;
    v_attempt integer;
    v_done boolean;
    v_failed text[] := '{}';
BEGIN
    FOREACH v_parent IN ARRAY ARRAY[
        'public.component_balance',
        'public.contract_storage',
        'public.protocol_state'
    ] LOOP
        v_done := false;
        v_attempt := 0;
        WHILE NOT v_done AND v_attempt < p_max_attempts LOOP
            v_attempt := v_attempt + 1;
            BEGIN
                -- Applies to the current transaction only. If the parent lock is not granted
                -- within the timeout, give up and retry instead of stalling the queries that
                -- queue behind the AccessExclusiveLock request.
                PERFORM set_config('lock_timeout', p_lock_timeout, true);
                PERFORM partman.drop_partition_time(
                    p_parent_table := v_parent,
                    p_retention := v_retention
                );
                v_done := true;
            EXCEPTION
                -- Retry on any error, not just lock failures: partman re-raises internal
                -- errors with a generic SQLSTATE, so lock timeouts cannot be told apart from
                -- other failures here. The call is idempotent, and a persistent error still
                -- surfaces through the RAISE below after p_max_attempts.
                WHEN OTHERS THEN
                    RAISE NOTICE 'drop_expired_partitions: % failed (attempt % of %): %',
                        v_parent, v_attempt, p_max_attempts, SQLERRM;
            END;
            -- One transaction per attempt: releases everything before the backoff sleep or
            -- the next table.
            COMMIT;
            IF NOT v_done AND v_attempt < p_max_attempts THEN
                PERFORM pg_sleep(30);
            END IF;
        END LOOP;
        IF NOT v_done THEN
            v_failed := v_failed || v_parent;
        END IF;
    END LOOP;

    -- Completed drops are already committed; raising here only marks the run as failed in
    -- cron.job_run_details so persistent errors are observable.
    IF array_length(v_failed, 1) > 0 THEN
        RAISE EXCEPTION 'drop_expired_partitions: failed for % after % attempts each, remaining partitions retry next run',
            v_failed, p_max_attempts;
    END IF;
END;
$$;

-- 00:30, after the (now creation-only) midnight run_maintenance and clear of the 02:00
-- transaction cleanup.
SELECT cron.schedule(
    'drop_expired_partitions',
    '30 0 * * *',
    'CALL drop_expired_partitions();'
);

-- Re-point the transaction cleanup horizon at the shared retention setting. Its default
-- p_min_age resolves from part_config.retention, which is NULL everywhere after the UPDATE
-- above, so the default would pin the horizon at the procedure's 31+4-day fallback and stop
-- tracking retention changes. Passing p_min_age explicitly in the cron command restores the
-- coupling without redefining the procedure. Manual runs should keep passing p_min_age
-- explicitly (see scripts/prune_transaction_table.md).
SELECT cron.unschedule(jobid)
FROM cron.job
WHERE command LIKE '%cleanup_orphaned_transactions%';

SELECT cron.schedule(
    'cleanup_orphaned_transactions',
    '0 2,14 * * *',
    'CALL cleanup_orphaned_transactions(p_min_age => partition_retention() + interval ''4 days'');'
);
