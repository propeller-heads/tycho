#!/usr/bin/env bash
# Prices newly ingested trades one chain at a time.
#   DSN=postgres://tycho:tycho@localhost:5433/router_trades ./scripts/price_trades.sh [interval_s]
# Set PRICE_ONCE=1 to run one pass and exit.
set -euo pipefail

DSN="${DSN:?set DSN to a libpq connection string}"
INTERVAL="${1:-60}"
MAX_AGE="${MAX_AGE:-1 hour}"
PRICE_MAX_AGE="${PRICE_MAX_AGE:-3 hours}"
DIR="$(cd "$(dirname "$0")/.." && pwd)"
SQL="$DIR/pricing/price_trades.sql"

price_all_chains() {
	local found=0
	local succeeded=0
	local chain
	while IFS= read -r chain; do
		found=1
		if psql "$DSN" -q -v ON_ERROR_STOP=1 -v chain="$chain" -v max_age="$MAX_AGE" \
			-v price_max_age="$PRICE_MAX_AGE" -f "$SQL"; then
			succeeded=1
		else
			echo "pricing failed for chain $chain; continuing" >&2
		fi
	done < <(
		psql "$DSN" -Atq -v ON_ERROR_STOP=1 -c \
			"SELECT substr(nspname, length('tycho_') + 1)
             FROM pg_namespace
             WHERE nspname LIKE 'tycho\_%' ESCAPE '\\'
             ORDER BY nspname"
	)
	if [ "$found" = 0 ]; then
		echo "no tycho_<chain> schemas found" >&2
		return 1
	fi
	[ "$succeeded" = 1 ]
}

while true; do
	if ! price_all_chains; then
		if [ "${PRICE_ONCE:-0}" = 1 ]; then
			exit 1
		fi
		echo "no chain could be priced; retrying" >&2
	fi
	if [ "${PRICE_ONCE:-0}" = 1 ]; then
		break
	fi
	sleep "$INTERVAL"
done
