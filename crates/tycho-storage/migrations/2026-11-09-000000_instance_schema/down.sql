-- Restores drop_expired_partitions() as defined in 2026-08-13_decouple_partition_retention.
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

SELECT cron.unschedule(jobid)
FROM cron.job
WHERE jobname = 'premake_history_partitions';

DELETE FROM partman.part_config
WHERE parent_table LIKE 'tycho.%';

DO $$
DECLARE
    v_table text;
BEGIN
    FOR v_table IN
        SELECT tablename FROM pg_tables
        WHERE schemaname = 'partman' AND tablename LIKE 'template_tycho_%'
    LOOP
        EXECUTE format('DROP TABLE partman.%I', v_table);
    END LOOP;
END;
$$;

ALTER TABLE public.token
    DROP COLUMN address;

ALTER TABLE public.protocol_system
    DROP COLUMN active_instance_id;

DROP SCHEMA tycho CASCADE;
