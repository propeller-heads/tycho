#!/usr/bin/env bash
# Tests the views in views.sql, in particular which rows trades_settled keeps.
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

psql_in <"$workspace_dir/schema.sql"
psql_in <"$workspace_dir/views.sql"
psql_in <"$workspace_dir/test_views.sql"

echo "Views integration test passed"
