#!/usr/bin/env bash
# Tests migrate_schema.sql on an older database, then the views in views.sql, in particular
# which rows trades_settled keeps.
set -euo pipefail

workspace_dir="$(cd "$(dirname "$0")/.." && pwd)"
db="router-trades-views-$$"

cleanup() {
	docker rm -f "$db" >/dev/null 2>&1 || true
}
trap cleanup EXIT

psql_in() {
	docker exec -i "$db" psql -v ON_ERROR_STOP=1 -q -U tycho -d router_trades
}

docker run --rm -d --name "$db" \
	-e POSTGRES_USER=tycho -e POSTGRES_PASSWORD=tycho -e POSTGRES_DB=router_trades \
	postgres:16 >/dev/null
# pg_isready answers before the init scripts finish, so wait for both.
for _ in {1..60}; do
	if [[ "$(docker logs "$db" 2>&1)" == *"init process complete; ready for start up."* ]] &&
		docker exec "$db" pg_isready -q -U tycho -d router_trades; then
		break
	fi
	sleep 1
done

# The pricer can start before any sink has created the tables.
psql_in <"$workspace_dir/migrate_schema.sql"
psql_in <"$workspace_dir/schema.sql"
# Stand in for a database an older package created: the tables without what the migration adds.
psql_in <<'SQL'
ALTER TABLE trades
    DROP COLUMN state_committed,
    DROP COLUMN router_positive_slippage_bps,
    DROP COLUMN client_positive_slippage_share_bps,
    DROP COLUMN positive_slippage_cap_bps,
    DROP COLUMN token_pair_fee;
ALTER TABLE fee_config_events DROP COLUMN token0, DROP COLUMN token1;
ALTER TABLE router_call_errors DROP COLUMN state_committed;
DROP INDEX IF EXISTS trades_tx_idx;
SQL
psql_in <"$workspace_dir/migrate_schema.sql"
# Every container applies the migration on start, so it has to apply on top of itself.
psql_in <"$workspace_dir/migrate_schema.sql"
psql_in <"$workspace_dir/test_migrate_schema.sql"
psql_in <"$workspace_dir/views.sql"
# Every container applies views.sql on start, so it has to apply on top of itself.
psql_in <"$workspace_dir/views.sql"
psql_in <"$workspace_dir/test_views.sql"

echo "Views integration test passed"
