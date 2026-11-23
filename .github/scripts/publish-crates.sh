#!/usr/bin/env bash
set -euo pipefail

# Publishes workspace crates to crates.io in the order given, which must be dependency order.
#
# A crate whose version is already on crates.io is skipped, so rerunning the release after a
# partial publish uploads only the crates still missing. After each upload the script waits until
# the version appears in the crates.io sparse index, the index cargo resolves the next crate's
# dependencies from, instead of sleeping a fixed time. An upload that fails is retried, because
# cargo refuses a crate whose dependency is uploaded but not yet indexed.
#
# Usage: publish-crates.sh <crate>...
# Reads the token from CARGO_REGISTRY_TOKEN, as cargo publish does.

INDEX_URL="https://index.crates.io"
PUBLISH_ATTEMPTS=5
RETRY_DELAY_SECONDS=30
INDEX_POLL_SECONDS=10
INDEX_TIMEOUT_SECONDS=600

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
# 404, which counts as not listed; any other HTTP error fails the script.
is_indexed() {
    local body status
    body=$(mktemp)
    status=$(curl -sS -o "$body" -w '%{http_code}' -H 'Cache-Control: no-cache' \
        "$INDEX_URL/$(index_path "$1")")
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

wait_until_indexed() {
    local waited=0
    until is_indexed "$1" "$2"; do
        if ((waited >= INDEX_TIMEOUT_SECONDS)); then
            echo "::error::$1 $2 is not in the crates.io index after ${INDEX_TIMEOUT_SECONDS}s" >&2
            exit 1
        fi
        sleep "$INDEX_POLL_SECONDS"
        waited=$((waited + INDEX_POLL_SECONDS))
    done
    echo "$1 $2 is in the crates.io index"
}

publish() {
    local attempt
    for ((attempt = 1; attempt <= PUBLISH_ATTEMPTS; attempt++)); do
        if cargo publish -p "$1" --locked; then
            return 0
        fi
        # An upload that went through but failed afterwards must not be retried.
        if is_indexed "$1" "$2"; then
            return 0
        fi
        echo "Publishing $1 $2 failed (attempt $attempt of $PUBLISH_ATTEMPTS)" >&2
        if ((attempt < PUBLISH_ATTEMPTS)); then
            sleep "$RETRY_DELAY_SECONDS"
        fi
    done
    echo "::error::Could not publish $1 $2 after $PUBLISH_ATTEMPTS attempts" >&2
    exit 1
}

for crate in "$@"; do
    version=$(crate_version "$crate")
    if is_indexed "$crate" "$version"; then
        echo "$crate $version is already on crates.io, skipping"
        continue
    fi
    publish "$crate" "$version"
    wait_until_indexed "$crate" "$version"
done
