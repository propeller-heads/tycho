#!/usr/bin/env bash
# Computes the raw EtherFi state that substreams.yaml carries in `params`.
#
# Reads every tracked storage slot at a given block, plus the hash of the block's first
# transaction to anchor the components to, and prints the JSON object the modules expect. The
# values are raw storage words so they decode through the same TrackedSlot definitions the
# update path uses.
#
# Usage:
#   RPC_URL=<archive-rpc> ./scripts/compute_initial_state.sh [block_number]

set -euo pipefail

BLOCK_NUMBER=${1:-25940000}

for bin in cast jq; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "Error: '$bin' is required but was not found in PATH." >&2
        exit 1
    fi
done
if [[ -z "${RPC_URL:-}" ]]; then
    echo "Error: RPC_URL must be set (Ethereum archive RPC)." >&2
    exit 1
fi

LIQUIDITY_POOL="0x308861A430be4cce5502d0A12724771Fc6DaF216"
EETH="0x35fA164735182de50811E8e2E824cFb9B6118ac2"
REDEMPTION_MANAGER="0xDadEf1fFBFeaAB4f68A9fD181395F68b4e4E7Ae0"
RATE_LIMITER="0x6C7c54cfC2225fA985cD25F04d923B93c60a02F8"

# Positions as declared in src/constants.rs.
LIQUIDITY_POOL_VALUE_POSITION="0xcf"
EETH_TOTAL_SHARES_POSITION="0xca"
WEETH_SHARES_POSITION="0x65699867c563473027a12e5ab944a50581e6a89508c194c0a9c647f5f7b8d911"
ETH_REDEMPTION_LIMIT_POSITION="0xde214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e4089"
ETH_REDEMPTION_INFO_POSITION="0xde214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e408a"
EETH_MINT_LIMIT_POSITION="0x307ba46f8ad0e50f846ef63910e4eaf48114447a0c6bed3a66bb4c9e19ac5c96"
EETH_BURN_LIMIT_POSITION="0x3f303c9df3b7d9b21f01cecce772249973e78530093cd404fcd79757440a074e"

read_storage() {
    cast storage "$1" "$2" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL"
}

echo "Reading EtherFi state at block $BLOCK_NUMBER..." >&2

creation_tx=$(cast block "$BLOCK_NUMBER" --json --rpc-url "$RPC_URL" | jq -r '.transactions[0]')
liquidity_pool_value=$(read_storage "$LIQUIDITY_POOL" "$LIQUIDITY_POOL_VALUE_POSITION")
eeth_total_shares=$(read_storage "$EETH" "$EETH_TOTAL_SHARES_POSITION")
weeth_shares=$(read_storage "$EETH" "$WEETH_SHARES_POSITION")
eth_redemption_limit=$(read_storage "$REDEMPTION_MANAGER" "$ETH_REDEMPTION_LIMIT_POSITION")
eth_redemption_info=$(read_storage "$REDEMPTION_MANAGER" "$ETH_REDEMPTION_INFO_POSITION")
eeth_mint_limit=$(read_storage "$RATE_LIMITER" "$EETH_MINT_LIMIT_POSITION")
eeth_burn_limit=$(read_storage "$RATE_LIMITER" "$EETH_BURN_LIMIT_POSITION")

cat <<EOF
{
  "start_block": $BLOCK_NUMBER,
  "creation_tx": "$creation_tx",
  "liquidity_pool_value": "$liquidity_pool_value",
  "eeth_total_shares": "$eeth_total_shares",
  "weeth_shares": "$weeth_shares",
  "eth_redemption_limit": "$eth_redemption_limit",
  "eth_redemption_info": "$eth_redemption_info",
  "eeth_mint_limit": "$eeth_mint_limit",
  "eeth_burn_limit": "$eeth_burn_limit"
}
EOF
