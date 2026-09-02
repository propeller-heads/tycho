-- Create upcoming partitions in short transactions
--
-- CREATE TABLE ... PARTITION OF takes an AccessExclusiveLock on the parent table, and
-- pg_partman's run_maintenance() acquires it inside its per-table maintenance transaction,
-- holding it until that transaction commits. Every query on the parent queues behind it for
-- the full transaction, not for the creation itself: reader stalls of up to 18 seconds at
-- midnight were measured (five extractor connections blocked in bind, all released at the
-- maintenance commit).
--
-- create_upcoming_partitions() below creates each missing premake partition through
-- partman.create_partition_time() in its own transaction, so the parent lock is held for
-- milliseconds per partition. part_config stays the source of truth for what should exist
-- (premake, partition_interval, which parents are managed), and partman's own
-- calculate_time_partition_info() snaps each target time to the partition boundary.
-- create_partition_time() returns false without taking the parent lock when the partition is
-- already there, so a normal run does nothing until the day rolls over.
--
-- Lock handling matches drop_expired_partitions: a per-transaction lock_timeout so a blocked
-- creation yields instead of stalling the readers queued behind it, retries with backoff,
-- retry on any error because partman re-raises internal errors with a generic SQLSTATE, and a
-- final RAISE so an exhausted run is visible in cron.job_run_details. The retry budget is
-- smaller than the drop job's: this procedure makes up to premake+1 calls per parent rather
-- than one, so the same budget would let a persistent failure run for many minutes.
--
-- run_maintenance_proc() stays scheduled as a safety net, moved to 01:00. With the premake
-- partitions already created at 00:05 it has nothing to create and takes no parent lock; it
-- only falls back to creating them (with the old stall) if create_upcoming_partitions()
-- failed, which its own failed cron run also reports.

CREATE OR REPLACE PROCEDURE create_upcoming_partitions(
    p_lock_timeout text DEFAULT '2s',
    p_max_attempts integer DEFAULT 3,
    p_retry_delay interval DEFAULT '10 seconds'
)
LANGUAGE plpgsql
AS $$
DECLARE
    v_parents text[];
    v_premakes int[];
    v_intervals interval[];
    v_parent text;
    v_target timestamptz;
    v_offset integer;
    v_created boolean;
    v_attempt integer;
    v_done boolean;
    v_failed text[] := '{}';
    i integer;
BEGIN
    -- Materialize the config before the loop: COMMIT is not allowed while a query cursor is
    -- open. Covers every partman-managed parent, including ones added later.
    SELECT array_agg(parent_table ORDER BY parent_table),
           array_agg(premake ORDER BY parent_table),
           array_agg(partition_interval::interval ORDER BY parent_table)
    INTO v_parents, v_premakes, v_intervals
    FROM partman.part_config
    WHERE automatic_maintenance = 'on';

    IF v_parents IS NULL THEN
        RETURN;
    END IF;

    FOR i IN 1..array_length(v_parents, 1) LOOP
        v_parent := v_parents[i];
        -- Today plus the configured premake window. Offsets are snapped to the partition
        -- boundary below; passing an unsnapped time makes partman build a range straddling
        -- two partitions, which then fails as an overlap.
        FOR v_offset IN 0..v_premakes[i] LOOP
            SELECT base_timestamp INTO v_target
            FROM partman.calculate_time_partition_info(
                v_intervals[i],
                clock_timestamp() + v_offset * v_intervals[i]
            );

            v_done := false;
            v_attempt := 0;
            WHILE NOT v_done AND v_attempt < p_max_attempts LOOP
                v_attempt := v_attempt + 1;
                BEGIN
                    -- Applies to the current transaction only: if the parent lock is not
                    -- granted within the timeout, yield instead of stalling the readers that
                    -- queue behind the AccessExclusiveLock request.
                    PERFORM set_config('lock_timeout', p_lock_timeout, true);
                    SELECT partman.create_partition_time(
                        p_parent_table := v_parent,
                        p_partition_times := ARRAY[v_target]
                    ) INTO v_created;
                    IF v_created THEN
                        RAISE NOTICE 'create_upcoming_partitions: created % partition for %',
                            v_parent, v_target;
                    END IF;
                    v_done := true;
                EXCEPTION
                    WHEN OTHERS THEN
                        RAISE NOTICE 'create_upcoming_partitions: % for % failed (attempt % of %): %',
                            v_parent, v_target, v_attempt, p_max_attempts, SQLERRM;
                END;
                -- One transaction per partition: releases the parent lock (or nothing, after
                -- a caught failure) before the backoff sleep or the next partition.
                COMMIT;
                IF NOT v_done AND v_attempt < p_max_attempts THEN
                    PERFORM pg_sleep(extract(epoch FROM p_retry_delay));
                END IF;
            END LOOP;
            IF NOT v_done THEN
                -- The remaining offsets need the same parent lock, so skip this parent; the
                -- 01:00 safety net or the next run picks them up.
                v_failed := v_failed || v_parent;
                EXIT;
            END IF;
        END LOOP;
    END LOOP;

    -- Completed creations are already committed; raising here only marks the run as failed
    -- in cron.job_run_details so persistent errors are observable.
    IF array_length(v_failed, 1) > 0 THEN
        RAISE EXCEPTION 'create_upcoming_partitions: failed for % after % attempts each, remaining partitions retry next run',
            v_failed, p_max_attempts;
    END IF;
END;
$$;

-- 00:05: right after the day rolls over, ahead of the 00:30 retention drops and the 01:00
-- safety net.
SELECT cron.schedule(
    'create_upcoming_partitions',
    '5 0 * * *',
    'CALL create_upcoming_partitions();'
);

-- Move run_maintenance from midnight (@daily) to 01:00: demoted to a safety net that
-- normally finds all premake partitions already created.
SELECT cron.unschedule(jobid)
FROM cron.job
WHERE command LIKE '%run_maintenance_proc%';

SELECT cron.schedule(
    'run_maintenance_safety_net',
    '0 1 * * *',
    'CALL partman.run_maintenance_proc();'
);
