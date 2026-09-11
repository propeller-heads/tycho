#!/bin/bash
# Script to compute the raw Lido V3 initial state used by substreams.yaml params.
#
# It reads the five tracked stETH proxy storage slots at a given block and prints
# the JSON object expected by the current substreams configuration.
#
# Usage:
#   ./scripts/compute_initial_state.sh [block_number]
#   LIDO_V3_RPC_URL=<your-archive-rpc> ./scripts/compute_initial_state.sh [block_number]
#   RPC_URL=<your-archive-rpc> ./scripts/compute_initial_state.sh [block_number]
#
# The script tries several endpoint environment variables in order and falls back
# to a public Ethereum RPC endpoint if needed.

set -euo pipefail

BLOCK_NUMBER=${1:-24083113}

if ! command -v cast >/dev/null 2>&1; then
  echo "Error: 'cast' is required but was not found in PATH." >&2
  exit 1
fi

STETH_PROXY="0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"

TOTAL_AND_EXTERNAL_SHARES_SLOT="0x6038150aecaa250d524370a0fdcdec13f2690e0723eaf277f41d7cae26b359e6"
BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_SLOT="0xa84c096ee27e195f25d7b6c7c2a03229e49f1a2a5087e57ce7d7127707942fe3"
CL_BALANCE_AND_CL_VALIDATORS_SLOT="0xc36804a03ec742b57b141e4e5d8d3bd1ddb08451fd0f9983af8aaab357a78e2f"
STAKING_STATE_SLOT="0xa3678de4a579be090bed1177e0a24f77cc29d181ac22fd7688aca344d8938015"
# shares[wstETH] in stETH's share mapping (mapping slot 0): cast index address <wstETH> 0
WSTETH_SHARES_SLOT="0xf37caed32e4e49c83636e0f1684f3f4a9a23c463a49eb17cd63abd50680b378b"

resolve_rpc_url() {
  local candidates=()

  if [ -n "${RPC_URL:-}" ]; then
    candidates+=("$RPC_URL")
  fi
  if [ -n "${ETH_RPC_URL:-}" ]; then
    candidates+=("$ETH_RPC_URL")
  fi
  candidates+=("https://ethereum-rpc.publicnode.com")

  local candidate
  for candidate in "${candidates[@]}"; do
    if cast block "$BLOCK_NUMBER" --rpc-url "$candidate" >/dev/null 2>&1; then
      echo "$candidate"
      return 0
    fi
  done

  echo "Error: could not find a working Ethereum RPC endpoint." >&2
  echo "Tried LIDO_V3_RPC_URL, RPC_URL, ETH_RPC_URL, TRACE_RPC_URL, and the public fallback." >&2
  exit 1
}

read_storage() {
  local contract=$1
  local slot=$2
  cast storage "$contract" "$slot" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL"
}

RPC_URL=$(resolve_rpc_url)

echo "Reading Lido V3 raw storage at block $BLOCK_NUMBER from $RPC_URL..." >&2

total_and_external_shares=$(read_storage "$STETH_PROXY" "$TOTAL_AND_EXTERNAL_SHARES_SLOT")
buffered_ether_and_deposited_validators=$(read_storage "$STETH_PROXY" "$BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_SLOT")
cl_balance_and_cl_validators=$(read_storage "$STETH_PROXY" "$CL_BALANCE_AND_CL_VALIDATORS_SLOT")
staking_state=$(read_storage "$STETH_PROXY" "$STAKING_STATE_SLOT")
wsteth_shares=$(read_storage "$STETH_PROXY" "$WSTETH_SHARES_SLOT")

cat <<EOF
{
  "start_block": $BLOCK_NUMBER,
  "total_and_external_shares": "$total_and_external_shares",
  "buffered_ether_and_deposited_validators": "$buffered_ether_and_deposited_validators",
  "cl_balance_and_cl_validators": "$cl_balance_and_cl_validators",
  "staking_state": "$staking_state",
  "wsteth_shares": "$wsteth_shares"
}
EOF
