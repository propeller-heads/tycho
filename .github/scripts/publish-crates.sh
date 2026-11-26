#!/usr/bin/env bash
set -euo pipefail

# Publishes workspace crates to crates.io with one cargo call, which uploads them in dependency
# order: every crate whose dependencies are already on crates.io goes up at once, and cargo waits
# for those to appear in the index before uploading the crates that depend on them.
#
# A crate whose version is already on crates.io is left out, so rerunning the release after a
# partial publish uploads only the crates still missing. A failed publish is retried with the
# crates still missing, because cargo can fail after uploading part of the set, or refuse a crate
# whose dependency is uploaded but not yet indexed.
#
# Usage: publish-crates.sh <crate>...
# Reads the token from CARGO_REGISTRY_TOKEN, as cargo publish does.

INDEX_URL="https://index.crates.io"
PUBLISH_ATTEMPTS=5
RETRY_DELAY_SECONDS=30

if [[ $# -eq 0 ]]; then
    echo "Usage: publish-crates.sh <crate>..." >&2
    exit 1
fi
: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN must be set}"

METADATA=$(cargo metadata --no-deps --format-version 1)

# The workspace version of a crate.
crate_version() {
    jq -er --arg name "$1" '.packages[] | select(.name == $name) | .version' <<<"$METADATA" || {
        echo "::error::$1 is not a package of this workspace" >&2
        exit 1
    }
}

# Path of a crate's file in the sparse index:
# https://doc.rust-lang.org/cargo/reference/registry-index.html#index-files
index_path() {
    local name
    name=$(tr '[:upper:]' '[:lower:]' <<<"$1")
    case ${#name} in
    1) echo "1/$name" ;;
    2) echo "2/$name" ;;
    3) echo "3/${name:0:1}/$name" ;;
    *) echo "${name:0:2}/${name:2:2}/$name" ;;
    esac
}

# Succeeds when crates.io's index lists version $2 of crate $1. A crate never published returns
# 404, which counts as not listed. curl retries a request that times out, is refused or gets a
# transient HTTP error (408, 429, 500, 502, 503, 504); any other HTTP error, or one that
# persists, fails the script.
is_indexed() {
    local body status
    body=$(mktemp)
    status=$(curl -sS --max-time 120 --retry 3 --retry-connrefused -o "$body" -w '%{http_code}' \
        -H 'Cache-Control: no-cache' "$INDEX_URL/$(index_path "$1")")
    case $status in
    200) jq -e --arg version "$2" 'select(.vers == $version)' "$body" >/dev/null ;;
    404) false ;;
    *)
        echo "::error::crates.io index returned HTTP $status for $1" >&2
        exit 1
        ;;
    esac
    local found=$?
    rm -f "$body"
    return $found
}

# Prints the given crates whose workspace version is not on crates.io yet.
missing_crates() {
    local crate version
    for crate in "$@"; do
        version=$(crate_version "$crate") || exit 1
        if is_indexed "$crate" "$version"; then
            echo "$crate $version is already on crates.io, skipping" >&2
        else
            echo "$crate"
        fi
    done
}

for ((attempt = 1; attempt <= PUBLISH_ATTEMPTS; attempt++)); do
    # An assignment, unlike a process substitution, fails the script when the check fails.
    missing_list=$(missing_crates "$@")
    if [[ -z $missing_list ]]; then
        echo "All crates are on crates.io"
        exit 0
    fi
    packages=()
    while IFS= read -r crate; do
        packages+=("--package=$crate")
    done <<<"$missing_list"
    if cargo publish --locked "${packages[@]}"; then
        exit 0
    fi
    echo "Publishing ${missing_list//$'\n'/ } failed (attempt $attempt of $PUBLISH_ATTEMPTS)" >&2
    if ((attempt < PUBLISH_ATTEMPTS)); then
        sleep "$RETRY_DELAY_SECONDS"
    fi
done
echo "::error::Could not publish all of $* after $PUBLISH_ATTEMPTS attempts" >&2
exit 1
