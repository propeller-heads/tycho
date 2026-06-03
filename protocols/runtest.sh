#!/usr/bin/env bash
set -euo pipefail

set -a
source ../.substreams.env
set +a

RPC_URL="${1:-https://wispy-hidden-knowledge.quiknode.pro/744cca7e0d6ab60a5bff9c19aee2599dbff70471}" \
RUST_LOG=protocol_testing=info,tycho_client=info,tycho_indexer=error,error \
PATH="/Users/indigo/projects/baseline/tycho-indexer/target/debug:$PATH" \
cargo run --bin protocol-testing -- range \
  --package ethereum-baseline \
  --chain ethereum \
  --match-test test_mainnet_pool_creation
