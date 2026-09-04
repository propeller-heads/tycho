#!/usr/bin/env bash
# Runs one sink (per chain) or the pricing loop.
#
#   entrypoint sink   CHAIN, SPKG, DSN, SUBSTREAMS_API_TOKEN; optional SUBSTREAMS_ENDPOINT,
#                     START_BLOCK, STOP_BLOCK, FLUSH_INTERVAL (default 100), METRICS_ADDR,
#                     TYCHO_S3_BUCKET, SPKG_CACHE_DIR
#   entrypoint price  DSN, TYCHO_<CHAIN>_DATABASE_URL...; optional MAX_AGE (default "1 hour"),
#                     INTERVAL (default 60), LOCAL_PRICING=1 for local stand-in tables
#
# Both modes wait for the database first, so the container can start together with Postgres.
# `sink` applies schema.sql with `substreams-sink-sql setup`; `price` registers the postgres_fdw
# servers (scripts/fdw_setup.sh). Both steps are idempotent.
set -euo pipefail

# SPKG is a local path when that file exists, otherwise a key in the release bucket. Same rule as
# the indexer's ensure_spkg, so a pinned S3 key and a bind-mounted file both work.
resolve_spkg() {
	local ref="$1" bucket dest
	if [ -f "$ref" ]; then
		printf '%s' "$ref"
		return
	fi
	bucket="${TYCHO_S3_BUCKET:-repo.propellerheads-propellerheads}"
	dest="${SPKG_CACHE_DIR:-/tmp/spkg}/$(basename "$ref")"
	if [ ! -f "$dest" ]; then
		mkdir -p "$(dirname "$dest")"
		echo "fetching s3://$bucket/$ref" >&2
		aws s3 cp "s3://$bucket/$ref" "$dest" >&2
	fi
	printf '%s' "$dest"
}

wait_for_db() {
	local uri="${1/psql:\/\//postgres://}"
	until pg_isready -q -d "$uri"; do
		echo "waiting for database" >&2
		sleep 2
	done
}

mode="${1:-sink}"
case "$mode" in
sink)
	: "${CHAIN:?set CHAIN (ethereum, base, ...)}"
	: "${SPKG:?set SPKG to a local path or a release key, e.g. substreams/tycho-router-trades/ethereum-v0.1.0.spkg}"
	: "${DSN:?set DSN, e.g. psql://user:pass@host:5432/db?sslmode=disable}"
	: "${SUBSTREAMS_API_TOKEN:?set SUBSTREAMS_API_TOKEN}"
	spkg=$(resolve_spkg "$SPKG")
	system_table_args=(
		--cursors-table "cursors_${CHAIN}"
		--history-table "substreams_history_${CHAIN}"
	)
	args=("$DSN" "$spkg")
	[ -n "${START_BLOCK:-}${STOP_BLOCK:-}" ] && args+=("${START_BLOCK:-}:${STOP_BLOCK:-}")
	[ -n "${SUBSTREAMS_ENDPOINT:-}" ] && args+=(-e "$SUBSTREAMS_ENDPOINT")
	wait_for_db "$DSN"
	substreams-sink-sql setup "$DSN" "$spkg" "${system_table_args[@]}"
	exec substreams-sink-sql run "${args[@]}" \
		"${system_table_args[@]}" \
		--batch-block-flush-interval "${FLUSH_INTERVAL:-100}" \
		--metrics-listen-addr "${METRICS_ADDR:-:9102}"
	;;
price)
	: "${DSN:?set DSN}"
	wait_for_db "$DSN"
	if [ "${LOCAL_PRICING:-0}" = 1 ]; then
		psql "$DSN" -q -v ON_ERROR_STOP=1 -v chain=ethereum \
			-f /opt/router-trades/pricing/dev_stub.sql
	else
		/opt/router-trades/scripts/fdw_setup.sh
	fi
	psql "$DSN" -q -v ON_ERROR_STOP=1 -f /opt/router-trades/pricing/migrate_usd.sql
	psql "$DSN" -q -v ON_ERROR_STOP=1 -f /opt/router-trades/pricing/preferred_tokens.sql
	exec /opt/router-trades/scripts/price_trades.sh "${INTERVAL:-60}"
	;;
*)
	exec "$@"
	;;
esac
