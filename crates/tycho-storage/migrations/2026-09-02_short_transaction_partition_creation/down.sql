-- Revert to partition creation inside run_maintenance: remove the short-transaction creation
-- job and restore the @daily schedule from 2024-06-25_v0.7.2.

SELECT cron.unschedule(jobid)
FROM cron.job
WHERE command LIKE '%create_upcoming_partitions%'
   OR command LIKE '%run_maintenance_proc%';

DROP PROCEDURE IF EXISTS create_upcoming_partitions(text, integer, interval);

SELECT cron.schedule('@daily', $$CALL partman.run_maintenance_proc()$$);
