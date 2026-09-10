#!/usr/bin/env bash
# Tests executors.sql: the seed, the views that name a hop, and what a re-run does to a
# hand-inserted executor.
set -euo pipefail

workspace_dir="$(cd "$(dirname "$0")/.." && pwd)"
db="router-trades-executors-$$"

cleanup() {
	docker rm -f "$db" >/dev/null 2>&1 || true
}
trap cleanup EXIT

psql_in() {
	docker exec -i "$db" psql -v ON_ERROR_STOP=1 -q -U tycho -d router_trades "$@"
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
psql_in <"$workspace_dir/executors.sql"
psql_in <"$workspace_dir/test_executors.sql"

# An executor deployed after the file was written, plus a hand edit of a row the file carries.
psql_in -c "INSERT INTO executors (chain, address, protocol_systems)
            VALUES ('ethereum', '0xdeadbeef00000000000000000000000000000000', ARRAY['brand_new_amm']);
            UPDATE executors SET protocol_systems = ARRAY['edited_by_hand']
            WHERE chain = 'ethereum' AND address = '0xfee95e97db5fdfcde672b9a06f4be87032dd7689';"
psql_in <"$workspace_dir/executors.sql"
psql_in <"$workspace_dir/test_executors_reapply.sql"

echo "Executor table integration test passed"
