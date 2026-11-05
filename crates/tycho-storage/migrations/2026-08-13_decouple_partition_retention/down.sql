-- Revert to coupled creation + retention: restore the part_config retention set by
-- 2024-09-16_v0.17.1_reduce_pg_partman_retention so run_maintenance() drops expired
-- partitions again, remove the separate drop job and retention config, and restore the
-- cleanup job's default horizon resolution (part_config-based again after the UPDATE below).

SELECT cron.unschedule(jobid)
FROM cron.job
WHERE command LIKE '%drop_expired_partitions%';

DROP PROCEDURE IF EXISTS drop_expired_partitions(interval, text, integer);

SELECT cron.unschedule(jobid)
FROM cron.job
WHERE command LIKE '%cleanup_orphaned_transactions%';

SELECT cron.schedule(
    'cleanup_orphaned_transactions',
    '0 2,14 * * *',
    'CALL cleanup_orphaned_transactions();'
);

DROP FUNCTION IF EXISTS partition_retention();

DROP TABLE IF EXISTS partition_retention_config;

UPDATE partman.part_config
SET retention = '1 month'
WHERE parent_table IN ('public.component_balance', 'public.contract_storage', 'public.protocol_state');
