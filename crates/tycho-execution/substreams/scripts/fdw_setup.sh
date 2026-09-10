#!/usr/bin/env bash
# Registers one postgres_fdw server per chain.
#
# Reads TYCHO_<CHAIN>_DATABASE_URL (e.g. TYCHO_ETHEREUM_DATABASE_URL=postgres://u:p@host:5432/db)
# for every chain and applies pricing/tycho_foreign_tables.sql with the parsed parts. Re-running
# replaces the servers, so it is safe on every container start.
#   DSN=postgres://tycho:...@localhost:5432/router_trades ./scripts/fdw_setup.sh
set -euo pipefail

DSN="${DSN:?set DSN to a libpq connection string}"
DIR="$(cd "$(dirname "$0")/.." && pwd)"

urldecode() { printf '%b' "${1//%/\\x}"; }

found=0
for var in $(compgen -A variable | grep -E '^TYCHO_[A-Z]+_DATABASE_URL$' || true); do
	chain=$(sed -E 's/^TYCHO_([A-Z]+)_DATABASE_URL$/\1/' <<<"$var" | tr '[:upper:]' '[:lower:]')
	url="${!var}"
	if [[ ! "$url" =~ ^postgres(ql)?://([^:/@]+):([^@]+)@([^:/]+)(:([0-9]+))?/([^?]+) ]]; then
		echo "$var is not a postgres://user:password@host[:port]/db URL" >&2
		exit 1
	fi
	echo "registering tycho_${chain} -> ${BASH_REMATCH[4]}"
	psql "$DSN" -q -v ON_ERROR_STOP=1 \
		-v chain="$chain" \
		-v tycho_host="${BASH_REMATCH[4]}" \
		-v tycho_port="${BASH_REMATCH[6]:-5432}" \
		-v tycho_db="${BASH_REMATCH[7]}" \
		-v tycho_user="$(urldecode "${BASH_REMATCH[2]}")" \
		-v tycho_password="$(urldecode "${BASH_REMATCH[3]}")" \
		-f "$DIR/pricing/tycho_foreign_tables.sql"
	found=1
done
if [ "$found" = 0 ]; then
	echo "no TYCHO_<CHAIN>_DATABASE_URL variables set; nothing to register" >&2
	exit 1
fi
