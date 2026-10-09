SELECT cron.unschedule(jobname)
FROM cron.job
WHERE jobname IN ('reindex_protocol_state_modify_tx', 'reindex_component_balance_modify_tx',
                  'reindex_watchdog', 'drop_invalid_indexes');
