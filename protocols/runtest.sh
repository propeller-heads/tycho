#!/usr/bin/env bash
set -euo pipefail

set -a
source ../.substreams.env
set +a

RPC_URL="${1:-${BASE_RPC_URL:-https://mainnet.base.org}}" \
RUST_LOG=protocol_testing=info,tycho_client=info,tycho_indexer=error,error \
PATH="/Users/indigo/projects/baseline/tycho-indexer/target/debug:$PATH" \
cargo run --bin protocol-testing -- range \
  --package base-baseline \
  --chain base \
  --match-test test_base_recent_pool_creation \
  --reuse-last-sync
