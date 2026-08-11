#!/bin/bash
# Run the ignored curve differential tests with RPC_URL from .rpc-url,
# scrubbing the URL from any output.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
cd "$dir"
rpc="$(cat .rpc-url)"
filter="${1:-differential_tricrypto_ng}"
RPC_URL="$rpc" cargo test -p tycho-simulation --lib curve -- --ignored "$filter" --nocapture 2>&1 \
  | sed "s#${rpc}#<RPC>#g" | tail -60
