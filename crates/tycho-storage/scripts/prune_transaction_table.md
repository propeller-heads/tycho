# Reclaiming `transaction` table disk space

This runbook replaces the old `prune_transaction_table.sql` rebuild-and-swap script (previously
kept only in the `dev-tycho/prune-tx-script` configmap). That script held `ACCESS EXCLUSIVE`
locks on `transaction` and all 7 referencing tables for its entire ~day-long run, freezing the
indexer fleet. It cannot be fixed incrementally on PostgreSQL 15: re-adding foreign keys on the
partitioned referencers (`component_balance`, `protocol_state`, `contract_storage`) does not
support `NOT VALID`, so their validation scans are forced inside the exclusive window.

## Division of labor

```
cleanup_orphaned_transactions()  daily pg_cron job: DELETEs orphan rows (logical cleanup)
autovacuum                       marks deleted space reusable (file stops growing)
this runbook                     physically compacts the file (disk returned to the OS)
```

Row deletion is NOT this runbook's job. Steady state needs no compaction at all: the daily
cleanup keeps the table at its floor and the file size plateaus. Run this only after something
structural changed — e.g. partman retention was shortened, or a large historic orphan backlog
was just deleted for the first time.

## Special case: after an initial sync / large backfill

Backfills orphan transactions immediately and in bulk: referencing rows carry historic
`valid_to` timestamps, fall outside the partman retention window, and vanish while the
transactions stay behind. The daily cron will NOT clean these up promptly — its horizon is
based on `inserted_ts` (DB insertion time), so backfilled rows look young for 35 days even
though they are already orphaned, and the 45-minute daily budget is sized for the steady-state
trickle, not a backlog of tens of millions of rows.

After (or during) a large sync, drain the backlog with aggressive manual passes instead.
`p_min_age` can be lowered safely: correctness comes from the `NOT EXISTS` probes, and block
flushes are atomic, so a committed transaction is never momentarily reference-less. Repeat
until a run reports `deleted 0 rows` twice in a row (one full cursor wrap):

```sql
CALL cleanup_orphaned_transactions(
    p_min_age     => interval '1 day',
    p_time_budget => interval '6 hours'
);
```

The same procedure serves both the daily cron and these manual drains — only the parameters
differ.

Timing trade-off: every batch commits in seconds and yields via `lock_timeout`, so running
during the sync is safe and keeps the backlog from accumulating (100M+ dangling rows), but it
competes with the sync for IO and slows it down. Running after lets the sync finish sooner but
allows the backlog to build. Default to draining after the sync; switch to during if table
growth or disk pressure becomes a concern.

For an extreme backlog (orphans well beyond the retained row count) during a scheduled
full-downtime window, a rebuild — copy referenced rows to a new table and drop the old one,
the retired configmap script's approach — does O(retained) work instead of O(orphans) deletes.
On PG 15 it needs the fleet at 0 anyway (partitioned referencers force FK revalidation inside
the exclusive window), so treat it as a last resort for maintenance windows only.

Once drained, continue below to reclaim the disk.

## Before either path

1. Drain orphans to the floor first, otherwise the compaction preserves rows that are about to
   be deleted anyway. Repeat until a run reports `deleted 0 rows`:

   ```sql
   CALL cleanup_orphaned_transactions(p_time_budget => interval '6 hours');
   ```

2. Check whether compaction is even worth it:

   ```sql
   SELECT pg_size_pretty(pg_total_relation_size('"transaction"')) AS on_disk,
          (SELECT count(*) FROM "transaction") AS live_rows,
          n_dead_tup
   FROM pg_stat_user_tables
   WHERE relname = 'transaction';
   ```

   Rough guide: `transaction` averages ~250 bytes/row including indexes. If `on_disk` is within
   ~30% of `live_rows x 250B`, there is little to reclaim — stop here.

3. Confirm free disk >= 2x the table's total size (both paths build a full copy before dropping
   the old file):

   ```sql
   SELECT pg_size_pretty(pg_total_relation_size('"transaction"'));
   -- compare against FreeStorageSpace in the RDS console/CloudWatch
   ```

## Path A — pg_repack (recommended: online, indexer keeps running)

pg_repack rewrites the table into a compact file while writes continue (a trigger logs
concurrent changes, a replay loop applies them), then swaps the physical file under the SAME
table OID. Foreign keys, indexes, and triggers keep pointing at the table untouched — no FK
drop/re-add, no PG 15 partitioned-FK limitation. Blocking: milliseconds at trigger install,
seconds at the final swap.

### Preconditions

- `CREATE EXTENSION pg_repack;` (RDS supports it on PG 15; needs rds_superuser).
- Client binary version MUST match the extension version:

  ```sql
  SELECT extversion FROM pg_extension WHERE extname = 'pg_repack';
  ```

  ```bash
  pg_repack --version
  ```

- The table has a primary key (`transaction_pkey` — satisfied).
- IO note: the copy phase competes with the indexer for the instance's IOPS budget. It holds
  no locks, but on the current undersized instances (db.m5.large dev / db.m5.xlarge prod are
  IOPS-burst-starved) expect elevated write latency for the duration. Prefer running after the
  planned instance resize.
- Lock note: the two brief `ACCESS EXCLUSIVE` acquisitions (trigger install and final swap)
  queue arriving queries behind them for up to `--wait-timeout` seconds per attempt.

### Run

From a pod with a version-matched client (run as a k8s Job, credentials from the usual secret):

```bash
pg_repack \
    --host "$DB_HOST" --dbname tycho_postgres --username tycho \
    --table transaction \
    --no-kill-backend \
    --wait-timeout 10 \
    --elevel INFO
```

- `--no-kill-backend` is MANDATORY. Without it, if the final swap cannot get its lock within
  `--wait-timeout`, pg_repack CANCELS the conflicting queries — i.e. it would kill indexer
  writes to win the lock. With it, pg_repack gives up instead; the indexer always has priority.
- If the swap times out (busy lock traffic), pg_repack aborts cleanly — just rerun the command.
- If a run is interrupted, rerunning cleans up its temporary objects. Leftovers live in the
  `repack` schema and are safe to drop manually if needed.

### After

```sql
ANALYZE "transaction";
SELECT pg_size_pretty(pg_total_relation_size('"transaction"'));
```

## Path B — VACUUM FULL (maintenance window, zero moving parts)

Takes `ACCESS EXCLUSIVE` on `transaction` for the whole rewrite (hours at this size on current
hardware). Only acceptable with the indexer fleet scaled to 0 — which is fine for dev.

```bash
kubectl -n <namespace> scale deploy <indexer-deployments> --replicas=0
```

```sql
VACUUM (FULL, ANALYZE) "transaction";
```

```bash
kubectl -n <namespace> scale deploy <indexer-deployments> --replicas=1
```

Interrupting it at any point is safe: the rewrite is transactional and rolls back atomically.
