#!/usr/bin/env bash
# Prices newly ingested trades one chain at a time, and refreshes vault_balances_snapshot every
# VAULT_SNAPSHOT_INTERVAL seconds (default 600).
#   DSN=postgres://tycho:tycho@localhost:5433/router_trades ./scripts/price_trades.sh [interval_s]
# Set PRICE_ONCE=1 to run one pass and exit.
set -euo pipefail

DSN="${DSN:?set DSN to a libpq connection string}"
INTERVAL="${1:-60}"
MAX_AGE="${MAX_AGE:-1 hour}"
PRICE_MAX_AGE="${PRICE_MAX_AGE:-3 hours}"
VAULT_SNAPSHOT_INTERVAL="${VAULT_SNAPSHOT_INTERVAL:-600}"
DIR="$(cd "$(dirname "$0")/.." && pwd)"
SQL="$DIR/pricing/price_trades.sql"
# Bash's seconds since start; the first pass is always due.
last_snapshot=-1

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

# Runs after pricing, so the snapshot values balances at the prices just written. A failure is
# logged and retried on the next pass; it does not stop pricing.
refresh_vault_snapshot() {
	if [ "$last_snapshot" -ge 0 ] && ((SECONDS - last_snapshot < VAULT_SNAPSHOT_INTERVAL)); then
		return
	fi
	if psql "$DSN" -q -v ON_ERROR_STOP=1 -c "SELECT refresh_vault_balances_snapshot()" \
		>/dev/null; then
		last_snapshot=$SECONDS
	else
		echo "vault balance snapshot failed; retrying next pass" >&2
	fi
}

while true; do
	if ! price_all_chains; then
		if [ "${PRICE_ONCE:-0}" = 1 ]; then
			exit 1
		fi
		echo "no chain could be priced; retrying" >&2
	fi
	refresh_vault_snapshot
	if [ "${PRICE_ONCE:-0}" = 1 ]; then
		break
	fi
	sleep "$INTERVAL"
done
