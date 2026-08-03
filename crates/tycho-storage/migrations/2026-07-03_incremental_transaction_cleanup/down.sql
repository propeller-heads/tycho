-- Revert to the previous cleanup: restore clean_transaction_table() as defined by
-- 2024-09-19_transaction_cleanup and its cron schedule from
-- 2025-10-24-reschedule-transaction-cleanup.

SELECT cron.unschedule(jobid)
FROM cron.job
WHERE command LIKE '%cleanup_orphaned_transactions%';

DROP PROCEDURE IF EXISTS cleanup_orphaned_transactions(bigint, interval, interval, text);

DROP TABLE IF EXISTS transaction_cleanup_progress;

CREATE OR REPLACE FUNCTION clean_transaction_table() RETURNS void AS $$
DECLARE
    batch_size INT := 1000;
    rows_deleted INT;
    offset_val INT := 0;
    orphaned_count INT;  -- Variable to store count of orphaned transactions
BEGIN
    -- Create the outer temporary table to hold all transaction IDs that are safe to delete
    CREATE TEMP TABLE temp_transaction_ids (
        id BIGINT PRIMARY KEY
    );

    RAISE NOTICE 'Searching for orphaned transactions';

    INSERT INTO temp_transaction_ids (id)
    SELECT t2.id
    FROM "transaction" t2;

    DELETE FROM temp_transaction_ids
    WHERE id IN (
        SELECT t2.id
        FROM temp_transaction_ids t2
        JOIN contract_code cc ON cc.modify_tx = t2.id
    );

	RAISE NOTICE 'Scanned contract_code';

    DELETE FROM temp_transaction_ids
    WHERE id IN (
        SELECT t2.id
        FROM temp_transaction_ids t2
        JOIN protocol_component pc ON pc.creation_tx = t2.id OR pc.deletion_tx = t2.id
    );

	RAISE NOTICE 'Scanned protocol_component';

    DELETE FROM temp_transaction_ids
    WHERE id IN (
        SELECT t2.id
        FROM temp_transaction_ids t2
        JOIN account a ON a.creation_tx = t2.id OR a.deletion_tx = t2.id
    );

	RAISE NOTICE 'Scanned account';

    DELETE FROM temp_transaction_ids
    WHERE id IN (
        SELECT t2.id
        FROM temp_transaction_ids t2
        JOIN account_balance ab ON ab.modify_tx = t2.id
    );

	RAISE NOTICE 'Scanned account_balance';

    DELETE FROM temp_transaction_ids
    WHERE id IN (
        SELECT t2.id
        FROM temp_transaction_ids t2
        JOIN component_balance cb ON cb.modify_tx = t2.id
    );

	RAISE NOTICE 'Scanned component_balance';

    DELETE FROM temp_transaction_ids
    WHERE id IN (
        SELECT t2.id
        FROM temp_transaction_ids t2
        JOIN protocol_state ps ON ps.modify_tx = t2.id
    );

	RAISE NOTICE 'Scanned protocol_state';

    DELETE FROM temp_transaction_ids
    WHERE id IN (
        SELECT t2.id
        FROM temp_transaction_ids t2
        JOIN contract_storage cs ON cs.modify_tx = t2.id
    );

	RAISE NOTICE 'Scanned contract_storage';

    SELECT COUNT(*) INTO orphaned_count FROM temp_transaction_ids;
    RAISE NOTICE 'Found % orphaned transactions', orphaned_count;


    -- Phase 2: Delete the collected transaction IDs in batches
    LOOP
        RAISE NOTICE 'Deleting next transaction batch. [batch_size = %]', batch_size;
        RAISE NOTICE 'Remaining transactions: %', orphaned_count;

        -- Delete the rows in batches from the transaction table
        DELETE FROM "transaction"
        WHERE id IN (
            SELECT id FROM temp_transaction_ids
            LIMIT batch_size
        );

        DELETE FROM temp_transaction_ids
        WHERE id IN (
            SELECT id FROM temp_transaction_ids
            LIMIT batch_size
        );

        GET DIAGNOSTICS rows_deleted = ROW_COUNT;

        orphaned_count := orphaned_count - rows_deleted;

        -- Exit the loop if no more transactions are left to delete
        IF rows_deleted < batch_size THEN
            RAISE NOTICE 'Transaction table cleanup complete.';
            EXIT;
        END IF;

        -- Pause between delete batches to reduce load
        PERFORM pg_sleep(1);
    END LOOP;

    -- Drop the temporary tables when done
    DROP TABLE IF EXISTS temp_transaction_ids;

END;
$$ LANGUAGE plpgsql;

SELECT cron.schedule('clean_transaction_table', '0 12 * * *', 'SELECT clean_transaction_table();');
