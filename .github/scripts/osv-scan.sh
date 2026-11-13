#!/usr/bin/env bash
set -euo pipefail

# Scans the crates a checkout builds and writes osv-scanner's JSON results, for osv-reporter to
# turn into a verdict. Found vulnerabilities are results, not a failure: the script fails only when
# osv-scanner itself does.
#
# The package list comes from osv-package-list.sh next to this script, whatever the checkout
# contains, so a checkout of an older commit is listed the same way. The checkout's own
# rust-toolchain.toml applies; rustup installs it if needed.
#
# Usage: osv-scan.sh <checkout> <config> <package-list> <results>
#   <config>        osv-scanner.toml to apply; /dev/null for none
#   <package-list>  where to write the list; osv-reporter matches findings by this path, so scans
#                   that are compared must use the same one
#   <results>       where to write the JSON results

usage="Usage: osv-scan.sh <checkout> <config> <package-list> <results>"
CHECKOUT="${1:?$usage}"
CONFIG="${2:?$usage}"
PACKAGE_LIST="${3:?$usage}"
RESULTS="${4:?$usage}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

(
    cd "$CHECKOUT"
    # Downloads only when the checkout pins a toolchain that is not installed yet. Its progress goes
    # to stderr, away from the package list.
    rustup toolchain install --no-self-update --profile minimal >&2
    "$script_dir/osv-package-list.sh"
) >"$PACKAGE_LIST"

# 0: no vulnerabilities, 1: vulnerabilities found. Anything else is an error.
status=0
osv-scanner scan source --format=json --output-file="$RESULTS" --config="$CONFIG" \
    --lockfile="osv-scanner:$PACKAGE_LIST" || status=$?
[ "$status" -le 1 ]
