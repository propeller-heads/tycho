#!/usr/bin/env bash
# Tests pricing against two independent Tycho source databases.
set -euo pipefail

workspace_dir="$(cd "$(dirname "$0")/.." && pwd)"
test_suffix="$$"
test_network="router-trades-test-${test_suffix}"
main_db="router-trades-main-${test_suffix}"
ethereum_db="router-trades-ethereum-${test_suffix}"
base_db="router-trades-base-${test_suffix}"

cleanup() {
	docker rm -f "$main_db" "$ethereum_db" "$base_db" >/dev/null 2>&1 || true
	docker network rm "$test_network" >/dev/null 2>&1 || true
}
trap cleanup EXIT

wait_for_postgres() {
	local container="$1"
	local database="$2"
	local logs
	for _ in {1..30}; do
		logs="$(docker logs "$container" 2>&1)"
		if [[ "$logs" == *"PostgreSQL init process complete; ready for start up."* ]] &&
			docker exec "$container" pg_isready -q -U tycho -d "$database"; then
			return
		fi
		sleep 1
	done
	echo "Postgres did not start in container $container" >&2
	return 1
}

start_postgres() {
	local container="$1"
	local database="$2"
	docker run --rm -d \
		--name "$container" \
		--network "$test_network" \
		-e POSTGRES_USER=tycho \
		-e POSTGRES_PASSWORD=tycho \
		-e POSTGRES_DB="$database" \
		postgres:16 >/dev/null
	wait_for_postgres "$container" "$database"
}

load_source_token() {
	local container="$1"
	local id="$2"
	local token_hex="$3"
	local decimals="$4"
	local price="$5"
	local age="$6"
	docker exec -i "$container" psql -v ON_ERROR_STOP=1 -U tycho -d tycho \
		-v id="$id" -v token_hex="$token_hex" -v decimals="$decimals" \
		-v price="$price" -v age="$age" <"$workspace_dir/pricing/test_source.sql"
}

docker network create "$test_network" >/dev/null
start_postgres "$main_db" router_trades
start_postgres "$ethereum_db" tycho
start_postgres "$base_db" tycho

# ethereum source: a stable that puts the native token at 2000 USD, and a token worth 2 native.
load_source_token "$ethereum_db" 1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 18 500000000000000000 "0 minutes"
load_source_token "$ethereum_db" 2 ffffffffffffffffffffffffffffffffffffffff 6 2000000000 "0 minutes"
# base source: a different anchor on purpose, 1000 USD per native token.
load_source_token "$base_db" 1 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 6 2000000 "0 minutes"
load_source_token "$base_db" 2 cccccccccccccccccccccccccccccccccccccccc 6 200000 "4 hours"
load_source_token "$base_db" 3 fff1111111111111111111111111111111111111 6 1000000000 "0 minutes"
load_source_token "$base_db" 4 eee1111111111111111111111111111111111111 6 500000 "0 minutes"
# pinned but priced far below its band, so it must be ignored rather than trusted.
load_source_token "$base_db" 5 deadbeef00000000000000000000000000000000 18 10000000000000000000 "0 minutes"

docker exec -i "$main_db" psql -v ON_ERROR_STOP=1 -U tycho -d router_trades \
	<"$workspace_dir/schema.sql"
docker exec -i "$main_db" psql -v ON_ERROR_STOP=1 -U tycho -d router_trades \
	<"$workspace_dir/pricing/preferred_tokens.sql"
docker exec -i "$main_db" psql -v ON_ERROR_STOP=1 -U tycho -d router_trades \
	<"$workspace_dir/pricing/test_preferred_tokens.sql"

docker run --rm --network "$test_network" \
	-v "$workspace_dir:/workspace:ro" \
	-e DSN="postgres://tycho:tycho@${main_db}:5432/router_trades" \
	-e TYCHO_ETHEREUM_DATABASE_URL="postgres://tycho:tycho@${ethereum_db}:5432/tycho" \
	-e TYCHO_BASE_DATABASE_URL="postgres://tycho:tycho@${base_db}:5432/tycho" \
	postgres:16 bash /workspace/scripts/fdw_setup.sh

docker exec -i "$main_db" psql -v ON_ERROR_STOP=1 -v max_age="1 hour" \
	-U tycho -d router_trades <"$workspace_dir/pricing/test_pricing.sql"
docker run --rm --network "$test_network" \
	-v "$workspace_dir:/workspace:ro" \
	-e DSN="postgres://tycho:tycho@${main_db}:5432/router_trades" \
	-e PRICE_ONCE=1 \
	postgres:16 bash /workspace/scripts/price_trades.sh
docker exec -i "$main_db" psql -v ON_ERROR_STOP=1 -U tycho -d router_trades \
	<"$workspace_dir/pricing/test_assertions.sql"

if [ "${1:-}" = "--outage-check" ]; then
	docker stop "$base_db" >/dev/null
	docker exec -i "$main_db" psql -v ON_ERROR_STOP=1 -U tycho -d router_trades \
		<"$workspace_dir/pricing/test_outage.sql"
	docker run --rm --network "$test_network" \
		-v "$workspace_dir:/workspace:ro" \
		-e DSN="postgres://tycho:tycho@${main_db}:5432/router_trades" \
		-e PRICE_ONCE=1 \
		postgres:16 bash /workspace/scripts/price_trades.sh
	docker exec -i "$main_db" psql -v ON_ERROR_STOP=1 -U tycho -d router_trades \
		<"$workspace_dir/pricing/test_outage_assertions.sql"
	echo "Healthy-chain pricing continued during the Base source outage"
fi

echo "Pricing integration test passed"
