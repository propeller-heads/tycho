-- Bound every reference probe of cleanup_orphaned_transactions to the batch's id range.
--
-- The probes target the partitioned tables (component_balance, protocol_state, contract_storage)
-- and modify_tx is not their partition key, so a correlated `x.modify_tx = t.id` cost one index
-- probe per partition per transaction: ~120-190 partitions across the three tables, 50k rows
-- per batch, for the whole 45-minute budget, twice a day. Adding `x.modify_tx > v_cursor AND
-- x.modify_tx <= v_batch_end` lets the planner run one short range scan per partition per batch
-- and an anti-join instead. Same rows qualify: the extra predicate is implied by the equality.
--
-- Measured on 2026-10-11, one 50k batch, identical orphan counts:
--   dev-base  3.17 s / 11.6M buffer hits  ->  54 ms / 476 buffers
--   dev-bsc  11.07 s / 14.2M buffer hits  ->  56 ms / 676 buffers
-- The procedure body is otherwise the one from 2026-07-03_incremental_transaction_cleanup.

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
              AND NOT EXISTS (SELECT 1 FROM contract_code cc WHERE cc.modify_tx = t.id
                               AND cc.modify_tx > v_cursor AND cc.modify_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM protocol_component pc WHERE pc.creation_tx = t.id
                               AND pc.creation_tx > v_cursor AND pc.creation_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM protocol_component pc WHERE pc.deletion_tx = t.id
                               AND pc.deletion_tx > v_cursor AND pc.deletion_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM account a WHERE a.creation_tx = t.id
                               AND a.creation_tx > v_cursor AND a.creation_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM account a WHERE a.deletion_tx = t.id
                               AND a.deletion_tx > v_cursor AND a.deletion_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM account_balance ab WHERE ab.modify_tx = t.id
                               AND ab.modify_tx > v_cursor AND ab.modify_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM component_balance cb WHERE cb.modify_tx = t.id
                               AND cb.modify_tx > v_cursor AND cb.modify_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM protocol_state ps WHERE ps.modify_tx = t.id
                               AND ps.modify_tx > v_cursor AND ps.modify_tx <= v_batch_end)
              AND NOT EXISTS (SELECT 1 FROM contract_storage cs WHERE cs.modify_tx = t.id
                               AND cs.modify_tx > v_cursor AND cs.modify_tx <= v_batch_end);
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
