#!/usr/bin/env bash
# Computes the raw EtherFi state that substreams.yaml carries in `params`.
#
# Reads the tracked storage slots plus the two component balances at a given block and prints
# the JSON object the modules expect. The values are raw storage words so they decode through
# the same StorageLocation definitions the update path uses.
#
# Usage:
#   RPC_URL=<archive-rpc> ./scripts/compute_initial_state.sh [block_number]

set -euo pipefail

BLOCK_NUMBER=${1:-25940000}

if ! command -v cast >/dev/null 2>&1; then
    echo "Error: 'cast' is required but was not found in PATH." >&2
    exit 1
fi
if [[ -z "${RPC_URL:-}" ]]; then
    echo "Error: RPC_URL must be set (Ethereum archive RPC)." >&2
    exit 1
fi

LIQUIDITY_POOL="0x308861A430be4cce5502d0A12724771Fc6DaF216"
EETH="0x35fA164735182de50811E8e2E824cFb9B6118ac2"
WEETH="0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee"
REDEMPTION_MANAGER="0xDadEf1fFBFeaAB4f68A9fD181395F68b4e4E7Ae0"

# LiquidityPool slot 0xcf packs totalValueOutOfLp and totalValueInLp; 0xdc holds
# ethAmountLockedForWithdrawl. eETH slot 0xca holds totalShares. The RedemptionManager keeps
# ethBucketLimiter and ethRedemptionInfo in two adjacent keccak-derived slots.
LIQUIDITY_POOL_VALUE_SLOT="0xcf"
LIQUIDITY_POOL_LOCKED_SLOT="0xdc"
EETH_TOTAL_SHARES_SLOT="0xca"
ETH_BUCKET_LIMITER_SLOT="0xde214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e4089"
ETH_REDEMPTION_INFO_SLOT="0xde214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e408a"

read_storage() {
    cast storage "$1" "$2" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL"
}

echo "Reading EtherFi state at block $BLOCK_NUMBER..." >&2

liquidity_pool_value_slot=$(read_storage "$LIQUIDITY_POOL" "$LIQUIDITY_POOL_VALUE_SLOT")
liquidity_pool_locked_slot=$(read_storage "$LIQUIDITY_POOL" "$LIQUIDITY_POOL_LOCKED_SLOT")
eeth_total_shares_slot=$(read_storage "$EETH" "$EETH_TOTAL_SHARES_SLOT")
eth_bucket_limiter_slot=$(read_storage "$REDEMPTION_MANAGER" "$ETH_BUCKET_LIMITER_SLOT")
eth_redemption_info_slot=$(read_storage "$REDEMPTION_MANAGER" "$ETH_REDEMPTION_INFO_SLOT")
liquidity_pool_native_balance=$(cast balance "$LIQUIDITY_POOL" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL")
weeth_eeth_balance=$(cast call "$EETH" "balanceOf(address)(uint256)" "$WEETH" \
    --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" | awk '{print $1}')

cat <<EOF
{
  "start_block": $BLOCK_NUMBER,
  "liquidity_pool_value_slot": "$liquidity_pool_value_slot",
  "liquidity_pool_locked_slot": "$liquidity_pool_locked_slot",
  "eeth_total_shares_slot": "$eeth_total_shares_slot",
  "eth_bucket_limiter_slot": "$eth_bucket_limiter_slot",
  "eth_redemption_info_slot": "$eth_redemption_info_slot",
  "liquidity_pool_native_balance": "$liquidity_pool_native_balance",
  "weeth_eeth_balance": "$weeth_eeth_balance"
}
EOF
