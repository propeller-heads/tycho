-- Incremental transaction cleanup
--
-- Replaces clean_transaction_table() with cleanup_orphaned_transactions().
--
-- The old function ran as a single transaction that anti-joined the whole transaction table
-- against every referencing table (O(all data), 10-28h on BSC volume). Its long-held
-- AccessShare locks on component_balance starved pg_partman's midnight partition maintenance
-- of the AccessExclusive lock it needs, which in turn queued the entire indexer behind
-- partman and repeatedly gridlocked (and once deadlocked) the database.
--
-- The new procedure deletes orphaned transaction rows in small id-range batches and COMMITs
-- after every batch, so no lock is held longer than a few seconds. A short lock_timeout makes
-- it yield to partman instead of queueing behind it. A persistent cursor makes runs resumable
-- after interruption, and a time budget bounds every invocation.
--
-- WARNING: the NOT EXISTS probes below are the correctness gate. On databases built purely
-- from migrations the referencing FKs are ON DELETE CASCADE, so deleting a referenced
-- transaction would silently cascade-delete protocol data; on environments where the legacy
-- rebuild script re-created the FKs they are plain NO ACTION, so the delete would error the
-- batch instead. If a new table ever adds a foreign key to "transaction", its column MUST be
-- added to the probe list (and indexed).

DROP FUNCTION IF EXISTS clean_transaction_table();

-- Single-row cursor so each run resumes where the previous one stopped.
CREATE TABLE IF NOT EXISTS transaction_cleanup_progress(
    id boolean PRIMARY KEY DEFAULT TRUE CHECK (id),
    last_processed_id bigint NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO transaction_cleanup_progress DEFAULT VALUES
ON CONFLICT DO NOTHING;

-- p_min_age: rows younger than this (by inserted_ts) are skipped. NULL (the default) resolves
-- to the largest pg_partman retention plus a 4-day margin for month-length variance and late
-- partition drops (35 days under the current 1-month retention), so the horizon tracks
-- retention changes automatically; 31+4 days is the fallback when partman has no retention
-- configured. NOTE: inserted_ts is DB insertion time, so during a backfill
-- freshly-inserted-but-already-orphaned rows are skipped until they age past the horizon;
-- drain those with manual runs using a lower p_min_age (see scripts/prune_transaction_table.md).
-- inserted_ts is used (rather than chain time) because the horizon binary search requires a
-- timestamp monotone in id order, which chain time is not when a newly added extractor
-- backfills alongside live ones.
CREATE OR REPLACE PROCEDURE cleanup_orphaned_transactions(
    p_batch_size bigint DEFAULT 50000,
    p_time_budget interval DEFAULT '45 minutes',
    p_min_age interval DEFAULT NULL,
    p_lock_timeout text DEFAULT '2s'
)
LANGUAGE plpgsql
AS $$
DECLARE
    v_started timestamptz := clock_timestamp();
    v_min_age interval;
    v_horizon_ts timestamptz;
    v_lo bigint;
    v_hi bigint;
    v_mid bigint;
    v_mid_ts timestamptz;
    v_horizon_id bigint;
    v_cursor bigint;
    v_batch_end bigint;
    v_batch_deleted bigint := 0;
    v_deleted bigint := 0;
    v_lock_failed boolean := false;
    v_lock_retries integer := 0;
    -- Every (table.column) referencing "transaction". Must stay in sync with the NOT EXISTS
    -- probes below; sorted in COLLATE "C" order for the safeguard comparison.
    v_expected_refs text[] := ARRAY[
        'account.creation_tx',
        'account.deletion_tx',
        'account_balance.modify_tx',
        'component_balance.modify_tx',
        'contract_code.modify_tx',
        'contract_storage.modify_tx',
        'protocol_component.creation_tx',
        'protocol_component.deletion_tx',
        'protocol_state.modify_tx'];
    v_actual_refs text[];
BEGIN
    IF p_min_age IS NOT NULL THEN
        v_min_age := p_min_age;
    ELSE
        SELECT coalesce(max(retention::interval), interval '31 days') + interval '4 days'
        INTO v_min_age
        FROM partman.part_config
        WHERE retention IS NOT NULL;
    END IF;
    v_horizon_ts := clock_timestamp() - v_min_age;

    -- Safeguard: refuse to run if the set of foreign keys referencing "transaction" no longer
    -- matches the probe list. Without this, a reference added by a future migration would let
    -- the sweep cascade-delete (or error on) rows the new table still needs.
    SELECT coalesce(array_agg(ref ORDER BY ref COLLATE "C"), '{}') INTO v_actual_refs
    FROM (
        SELECT DISTINCT r.relname || '.' || a.attname AS ref
        FROM pg_constraint c
        JOIN pg_class r ON r.oid = c.conrelid
        CROSS JOIN LATERAL unnest(c.conkey) AS k(attnum)
        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
        WHERE c.contype = 'f'
          AND c.confrelid = '"transaction"'::regclass
          AND c.conparentid = 0
    ) refs;
    IF v_actual_refs <> v_expected_refs THEN
        RAISE EXCEPTION
            'foreign keys referencing "transaction" (%) do not match the probe list (%); update cleanup_orphaned_transactions before running',
            v_actual_refs, v_expected_refs;
    END IF;

    SELECT min(id), max(id) INTO v_lo, v_hi FROM "transaction";
    IF v_lo IS NULL THEN
        RAISE NOTICE 'cleanup_orphaned_transactions: transaction table is empty';
        RETURN;
    END IF;

    -- Binary-search the largest id older than the horizon (~40 single-row PK lookups).
    -- Under live indexing, rows younger than the retention window cannot be orphans yet (their
    -- references cannot have been partition-dropped). During backfills young rows CAN already
    -- be orphans; they are deliberately deferred until they age past the horizon (see the
    -- p_min_age note above and the runbook). The NOT EXISTS checks below are the correctness
    -- gate; this is only an optimization.
    WHILE v_lo < v_hi LOOP
        v_mid := (v_lo + v_hi + 1) / 2;
        SELECT inserted_ts INTO v_mid_ts
        FROM "transaction"
        WHERE id >= v_mid
        ORDER BY id
        LIMIT 1;
        IF v_mid_ts IS NOT NULL AND v_mid_ts < v_horizon_ts THEN
            v_lo := v_mid;
        ELSE
            v_hi := v_mid - 1;
        END IF;
    END LOOP;
    v_horizon_id := v_lo;

    -- The search only verifies ids it moved v_lo onto; the initial v_lo = min(id) is never
    -- checked. If every row is younger than the horizon the loop converges there unverified,
    -- so re-probe the boundary and bail out rather than sweep a too-young row.
    SELECT inserted_ts INTO v_mid_ts
    FROM "transaction"
    WHERE id >= v_horizon_id
    ORDER BY id
    LIMIT 1;
    IF v_mid_ts IS NULL OR v_mid_ts >= v_horizon_ts THEN
        RAISE NOTICE 'cleanup_orphaned_transactions: no rows older than %, nothing to do',
            v_min_age;
        RETURN;
    END IF;

    SELECT last_processed_id INTO v_cursor FROM transaction_cleanup_progress;
    IF v_cursor IS NULL THEN
        -- Self-heal if the progress row is missing (e.g. wiped by test teardown).
        INSERT INTO transaction_cleanup_progress DEFAULT VALUES
        ON CONFLICT DO NOTHING;
        v_cursor := 0;
    END IF;
    IF v_cursor >= v_horizon_id THEN
        -- Previous sweep reached the horizon: wrap around. Orphans appear anywhere below the
        -- horizon, not just in the newest band: versioned rows move to a dated partition when
        -- their valid_to is set, and that partition is dropped a month later, orphaning
        -- transactions of any age.
        v_cursor := 0;
    END IF;
    -- Skip id space below the smallest existing row (earlier sweeps already emptied it).
    SELECT min(id) - 1 INTO v_lo FROM "transaction";
    v_cursor := greatest(v_cursor, v_lo);

    WHILE v_cursor < v_horizon_id AND clock_timestamp() - v_started < p_time_budget LOOP
        v_batch_end := least(v_cursor + p_batch_size, v_horizon_id);
        BEGIN
            -- Applies to the current batch transaction only. If partman or anything else
            -- holds a conflicting lock, the batch aborts and the procedure yields instead of
            -- queueing behind it (a queued AccessExclusive would freeze the indexer).
            PERFORM set_config('lock_timeout', p_lock_timeout, true);

            DELETE FROM "transaction" t
            WHERE t.id > v_cursor
              AND t.id <= v_batch_end
              AND NOT EXISTS (SELECT 1 FROM contract_code cc WHERE cc.modify_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM protocol_component pc WHERE pc.creation_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM protocol_component pc WHERE pc.deletion_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM account a WHERE a.creation_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM account a WHERE a.deletion_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM account_balance ab WHERE ab.modify_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM component_balance cb WHERE cb.modify_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM protocol_state ps WHERE ps.modify_tx = t.id)
              AND NOT EXISTS (SELECT 1 FROM contract_storage cs WHERE cs.modify_tx = t.id);
            GET DIAGNOSTICS v_batch_deleted = ROW_COUNT;
        EXCEPTION
            WHEN lock_not_available OR deadlock_detected THEN
                v_lock_failed := true;
        END;

        IF v_lock_failed THEN
            v_lock_failed := false;
            v_lock_retries := v_lock_retries + 1;
            IF v_lock_retries >= 3 THEN
                RAISE NOTICE 'cleanup_orphaned_transactions: lock not available after % attempts, yielding at id %',
                    v_lock_retries, v_cursor;
                EXIT;
            END IF;
            -- The failed batch's subtransaction already rolled back; end the enclosing
            -- transaction too so nothing (not even a snapshot) is held while backing off.
            COMMIT;
            PERFORM pg_sleep(30);
            CONTINUE;
        END IF;
        v_lock_retries := 0;

        v_deleted := v_deleted + v_batch_deleted;
        v_cursor := v_batch_end;
        UPDATE transaction_cleanup_progress
        SET last_processed_id = v_cursor,
            updated_at = clock_timestamp();
        -- Releases every lock taken by this batch and persists cursor progress. Interrupting
        -- the procedure at any point loses at most the current (uncommitted) batch.
        COMMIT;
    END LOOP;

    RAISE NOTICE 'cleanup_orphaned_transactions: deleted % rows, cursor at % (horizon %)',
        v_deleted, v_cursor, v_horizon_id;
END;
$$;

-- Replace the old cron entry. Match by command rather than jobid: ids differ per environment.
SELECT cron.unschedule(jobid)
FROM cron.job
WHERE command LIKE '%clean_transaction_table%';

-- Twice daily, clear of partman's midnight maintenance (with the per-batch lock_timeout even
-- an overlap is harmless). Two 45-minute runs give roughly 3-10M deletions/day of capacity,
-- comfortable margin over the ~2M/day steady-state orphan rate.
SELECT cron.schedule(
    'cleanup_orphaned_transactions',
    '0 2,14 * * *',
    'CALL cleanup_orphaned_transactions();'
);
