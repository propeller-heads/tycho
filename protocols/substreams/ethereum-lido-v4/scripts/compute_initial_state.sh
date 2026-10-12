#!/usr/bin/env bash
# Prints the `params` snapshot for substreams.yaml: the five stETH storage slots the package
# tracks, read at one block. The supported Lido v4 layout requires block 25603297 or later.
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

for bin in cast jq bc; do
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

echo "Reading stETH raw storage at block $BLOCK_NUMBER..." >&2

total_and_external_shares=$(read_storage "$STETH_PROXY" "$TOTAL_AND_EXTERNAL_SHARES_SLOT")
buffered_ether_and_deposited_post_report=$(read_storage "$STETH_PROXY" "$BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_SLOT")
cl_validators_balance_and_cl_pending_balance=$(read_storage "$STETH_PROXY" "$CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_SLOT")
staking_state=$(read_storage "$STETH_PROXY" "$STAKING_STATE_SLOT")
wsteth_shares=$(read_storage "$STETH_PROXY" "$WSTETH_SHARES_SLOT")

# Anchor creation to the last transaction emitting a stETH log in the start block.
# The snapshot contains end-of-block storage; the start block emits only component creation.
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

# Validate every tracked slot against stETH's getters before printing the snapshot.
# The slot layout must match the recorded implementation.
# The implementation the slot positions in src/constants.rs were verified against. The package
# pauses its component when the proxy moves off the implementation the manifest records, so
# after an upgrade: re-verify every slot, update this constant, and take a fresh snapshot.
VERIFIED_STETH_IMPLEMENTATION="0x028271E30a695c0527A0C50cA30603feD004cDb0"
WSTETH="0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"
FAILURES=0

check() {
  if [ "$2" != "$3" ]; then
    echo "MISMATCH $1: chain says $2, the slots decode to $3" >&2
    FAILURES=$((FAILURES + 1))
  fi
}

# Decimal value of a hex slice of a 32-byte word, counted in hex characters from the left.
slice() {
  local word="${1#0x}"
  cast to-dec "0x${word:$2:$3}"
}

call() {
  cast call "$STETH_PROXY" "$1" ${2:+"$2"} --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" |
    awk '{print $1}'
}

echo "Verifying the tracked slots against the chain..." >&2

live_implementation=$(call 'implementation()(address)')
if [ "$(echo "$live_implementation" | tr '[:upper:]' '[:lower:]')" != \
  "$(echo "$VERIFIED_STETH_IMPLEMENTATION" | tr '[:upper:]' '[:lower:]')" ]; then
  echo "UPGRADED stETH: $STETH_PROXY now runs $live_implementation, not $VERIFIED_STETH_IMPLEMENTATION." >&2
  echo "  Re-verify every slot in src/constants.rs against the new implementation." >&2
  FAILURES=$((FAILURES + 1))
fi

check "getTotalShares" "$(call 'getTotalShares()(uint256)')" \
  "$(slice "$total_and_external_shares" 32 32)"
check "getExternalShares" "$(call 'getExternalShares()(uint256)')" \
  "$(slice "$total_and_external_shares" 0 32)"
check "getBufferedEther" "$(call 'getBufferedEther()(uint256)')" \
  "$(slice "$buffered_ether_and_deposited_post_report" 32 32)"
check "sharesOf(wstETH)" "$(call 'sharesOf(address)(uint256)' "$WSTETH")" \
  "$(slice "$wsteth_shares" 0 64)"

# getTotalPooledEther() is the whole reason the first three slots are tracked, so it is checked
# against the sum the package reconstructs rather than against any single slot.
buffered=$(slice "$buffered_ether_and_deposited_post_report" 32 32)
deposited=$(slice "$buffered_ether_and_deposited_post_report" 0 32)
cl_balance=$(slice "$cl_validators_balance_and_cl_pending_balance" 32 32)
cl_pending=$(slice "$cl_validators_balance_and_cl_pending_balance" 0 32)
total_shares=$(slice "$total_and_external_shares" 32 32)
external_shares=$(slice "$total_and_external_shares" 0 32)
internal=$(echo "$buffered + $cl_balance + $cl_pending + $deposited" | bc)
internal_shares=$(echo "$total_shares - $external_shares" | bc)
pooled=$(echo "$internal + $external_shares * $internal / $internal_shares" | bc)
check "getTotalPooledEther" "$(call 'getTotalPooledEther()(uint256)')" "$pooled"

# The staking word packs the four StakeLimitUtils fields that bound ETH -> stETH.
# getStakeLimitFullInfo() returns all four, so each is checked against the slice it is unpacked
# from. Its third return value, the limit accrued so far, is derived from them rather than
# stored, so it is not one of the slices.
stake_limit_info=$(cast call "$STETH_PROXY" \
  'getStakeLimitFullInfo()(bool,bool,uint256,uint256,uint256,uint256,uint256)' \
  --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" | awk '{print $1}')
read -r _is_paused _is_set _current max_stake_limit max_growth_blocks prev_stake_limit \
  prev_stake_block_number <<<"$(echo "$stake_limit_info" | tr '\n' ' ')"
check "maxStakeLimit" "$max_stake_limit" "$(slice "$staking_state" 0 24)"
check "maxStakeLimitGrowthBlocks" "$max_growth_blocks" "$(slice "$staking_state" 24 8)"
check "prevStakeLimit" "$prev_stake_limit" "$(slice "$staking_state" 32 24)"
check "prevStakeBlockNumber" "$prev_stake_block_number" "$(slice "$staking_state" 56 8)"

if [ "$FAILURES" -gt 0 ]; then
  echo "$FAILURES checks failed; the snapshot was not printed." >&2
  exit 1
fi
echo "All tracked slots agree with the chain." >&2

cat <<JSON
{
  "start_block": $BLOCK_NUMBER,
  "total_and_external_shares": "$total_and_external_shares",
  "buffered_ether_and_deposited_post_report": "$buffered_ether_and_deposited_post_report",
  "cl_validators_balance_and_cl_pending_balance": "$cl_validators_balance_and_cl_pending_balance",
  "staking_state": "$staking_state",
  "wsteth_shares": "$wsteth_shares",
  "creation_tx": "$creation_tx",
  "implementations": {
    "steth": "$(echo "$live_implementation" | tr '[:upper:]' '[:lower:]')"
  }
}
JSON
