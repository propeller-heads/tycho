#!/usr/bin/env bash
set -euo pipefail

# Fails when osv-scanner reports a problem with osv-scanner.toml: an ignore that no longer matches
# the package list, an ignore past its expiry date, a deprecated key, or a config it cannot parse.
#
# osv-scanner reports unused and expired ignores only as warnings and never through its exit code.
# At --verbosity=warn a scan with a clean config writes nothing to stderr, so any output there is a
# finding. Vulnerabilities are not this script's concern: osv-scanner exits 1 when it finds any,
# and the scan jobs in security.yaml gate on those.
#
# Usage:
#   package_list="$(mktemp)"
#   osv-package-list.sh >"$package_list"
#   check-osv-scanner-config.sh "$package_list"
#
# Exit codes:
#   0 — the config is clean
#   1 — osv-scanner wrote to stderr or failed (its output is printed)

PACKAGE_LIST="${1:?Usage: check-osv-scanner-config.sh <package-list>}"
# osv-scanner otherwise applies the osv-scanner.toml next to the scanned file, which a package list
# in a temporary directory does not have.
CONFIG="$(dirname "${BASH_SOURCE[0]}")/../../osv-scanner.toml"

stderr_file="$(mktemp)"
trap 'rm -f "$stderr_file"' EXIT

status=0
osv-scanner scan source --verbosity=warn --format=json --output-file=/dev/null \
    --config="$CONFIG" --lockfile="osv-scanner:$PACKAGE_LIST" 2>"$stderr_file" || status=$?

# 0: no vulnerabilities, 1: vulnerabilities found. Anything else is an error, e.g. 130 for an
# invalid config.
if [ "$status" -gt 1 ] || [ -s "$stderr_file" ]; then
    cat "$stderr_file" >&2
    echo "osv-scanner exited with $status; fix osv-scanner.toml (see the output above)" >&2
    exit 1
fi
