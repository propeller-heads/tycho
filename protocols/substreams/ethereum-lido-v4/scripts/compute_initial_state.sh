#!/usr/bin/env bash
# Prints the `params` snapshot for substreams.yaml: the five stETH storage slots the package
# tracks, read at one block. Lido v4 (block 25603297 onwards) moved the pooled-ether accounting
# to new slots, so the snapshot has to be taken at or after that block.
#
# Usage:
#   RPC_URL=<archive-rpc> ./scripts/compute_initial_state.sh [block_number]
#
# RPC_URL must be an archive node: the script reads storage at a past block, which a public
# endpoint will refuse.

set -euo pipefail

BLOCK_NUMBER=${1:-25603297}

if [ -z "${RPC_URL:-}" ]; then
  echo "Error: RPC_URL must be set to an archive Ethereum RPC." >&2
  exit 1
fi

for bin in cast jq; do
  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "Error: '$bin' is required but was not found in PATH." >&2
    exit 1
  fi
done

STETH_PROXY="0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"

# keccak256 of the names Lido.sol v4.0.0 documents next to each position constant.
TOTAL_AND_EXTERNAL_SHARES_SLOT="0x6038150aecaa250d524370a0fdcdec13f2690e0723eaf277f41d7cae26b359e6"
BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_SLOT="0x81a11fa1111afa59b50051f60ccf604a39d96acb484dc467ad8eadb4a63f0a5f"
CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_SLOT="0x096e465397f38e659238ccd5d5a2c434ced54a63fd8d694045bfb058ab9d8112"
STAKING_STATE_SLOT="0xa3678de4a579be090bed1177e0a24f77cc29d181ac22fd7688aca344d8938015"
# shares[wstETH] in stETH's share mapping (mapping slot 0): cast index address <wstETH> 0
WSTETH_SHARES_SLOT="0xf37caed32e4e49c83636e0f1684f3f4a9a23c463a49eb17cd63abd50680b378b"

read_storage() {
  local contract=$1
  local slot=$2
  cast storage "$contract" "$slot" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL"
}

echo "Reading stETH raw storage at block $BLOCK_NUMBER from $RPC_URL..." >&2

total_and_external_shares=$(read_storage "$STETH_PROXY" "$TOTAL_AND_EXTERNAL_SHARES_SLOT")
buffered_ether_and_deposited_post_report=$(read_storage "$STETH_PROXY" "$BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_SLOT")
cl_validators_balance_and_cl_pending_balance=$(read_storage "$STETH_PROXY" "$CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_SLOT")
staking_state=$(read_storage "$STETH_PROXY" "$STAKING_STATE_SLOT")
wsteth_shares=$(read_storage "$STETH_PROXY" "$WSTETH_SHARES_SLOT")

# The component has no creation event, so it is anchored to a transaction in the start block.
# The last one that touches stETH is the anchor: the storage above is read at the end of the
# block, and map_protocol_changes takes either the creation branch or the update branch, never
# both, so nothing replays intra-block writes. At the v4 migration block that transaction is the
# DAO vote that ran finalizeUpgrade_v4.
block_hex=$(printf '0x%x' "$BLOCK_NUMBER")
creation_tx=$(
  cast rpc eth_getLogs \
    "{\"fromBlock\":\"$block_hex\",\"toBlock\":\"$block_hex\",\"address\":\"$STETH_PROXY\"}" \
    --rpc-url "$RPC_URL" | jq -r '.[-1].transactionHash // empty'
)
if [ -z "$creation_tx" ]; then
  echo "Error: block $BLOCK_NUMBER has no stETH logs to anchor the component to." >&2
  exit 1
fi

cat <<JSON
{
  "start_block": $BLOCK_NUMBER,
  "total_and_external_shares": "$total_and_external_shares",
  "buffered_ether_and_deposited_post_report": "$buffered_ether_and_deposited_post_report",
  "cl_validators_balance_and_cl_pending_balance": "$cl_validators_balance_and_cl_pending_balance",
  "staking_state": "$staking_state",
  "wsteth_shares": "$wsteth_shares",
  "creation_tx": "$creation_tx"
}
JSON
