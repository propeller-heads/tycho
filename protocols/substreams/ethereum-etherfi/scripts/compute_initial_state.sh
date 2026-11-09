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
WEETH="0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee"
REDEMPTION_MANAGER="0xDadEf1fFBFeaAB4f68A9fD181395F68b4e4E7Ae0"
RATE_LIMITER="0x6C7c54cfC2225fA985cD25F04d923B93c60a02F8"
ETH_SENTINEL="0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"

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

# Anchor component creation to the start block's first transaction for reproducibility.
# The snapshot contains end-of-block storage; the start block emits only component creation.
creation_tx=$(cast block "$BLOCK_NUMBER" --json --rpc-url "$RPC_URL" | jq -r '.transactions[0]')
liquidity_pool_value=$(read_storage "$LIQUIDITY_POOL" "$LIQUIDITY_POOL_VALUE_POSITION")
eeth_total_shares=$(read_storage "$EETH" "$EETH_TOTAL_SHARES_POSITION")
weeth_shares=$(read_storage "$EETH" "$WEETH_SHARES_POSITION")
eth_redemption_limit=$(read_storage "$REDEMPTION_MANAGER" "$ETH_REDEMPTION_LIMIT_POSITION")
eth_redemption_info=$(read_storage "$REDEMPTION_MANAGER" "$ETH_REDEMPTION_INFO_POSITION")
eeth_mint_limit=$(read_storage "$RATE_LIMITER" "$EETH_MINT_LIMIT_POSITION")
eeth_burn_limit=$(read_storage "$RATE_LIMITER" "$EETH_BURN_LIMIT_POSITION")

# Validate every tracked slot against its contract getter before printing the snapshot.
# Slot layouts are specific to the recorded implementations; a valid storage word alone
# does not establish that the slot still represents the expected field.
EIP1967_IMPLEMENTATION="0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
FAILURES=0

check() {
    local what="$1" want="$2" got="$3"
    if [[ "$(printf '%s' "$want" | tr '[:upper:]' '[:lower:]')" != \
          "$(printf '%s' "$got" | tr '[:upper:]' '[:lower:]')" ]]; then
        echo "MISMATCH $what: chain says $want, the slots decode to $got" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

# The implementation behind an EIP-1967 proxy, lower-cased.
implementation_of() {
    local word
    word=$(read_storage "$1" "$EIP1967_IMPLEMENTATION")
    echo "0x${word: -40}" | tr '[:upper:]' '[:lower:]'
}

# The package pauses its components when a proxy moves off the implementation the manifest
# records, so after an upgrade: re-verify every slot, update the expected address here, and take
# a fresh snapshot.
check_implementation() {
    local name="$1" proxy="$2" live="$3" expected="$4"
    if [[ "$live" != "$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')" ]]; then
        echo "UPGRADED $name: $proxy now runs $live, not $expected." >&2
        echo "  Re-verify the tracked slots and simulation behavior against the new implementation." >&2
        FAILURES=$((FAILURES + 1))
    fi
}

# Decimal value of a hex slice of a 32-byte word, counted in hex characters from the left.
slice() {
    local word="${1#0x}"
    cast to-dec "0x${word:$2:$3}"
}

call() {
    cast call "$1" "$2" ${3:+"$3"} --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" | awk '{print $1}'
}

echo "Verifying the tracked slots against the chain..." >&2

liquidity_pool_implementation=$(implementation_of "$LIQUIDITY_POOL")
eeth_implementation=$(implementation_of "$EETH")
weeth_implementation=$(implementation_of "$WEETH")
redemption_manager_implementation=$(implementation_of "$REDEMPTION_MANAGER")
rate_limiter_implementation=$(implementation_of "$RATE_LIMITER")

check_implementation "LiquidityPool" "$LIQUIDITY_POOL" "$liquidity_pool_implementation" \
    "0x17a16747d03006c9754548ac0d0aff48783a4a45"
check_implementation "eETH" "$EETH" "$eeth_implementation" \
    "0xd1901dd36cbf4a81386d0162df2707f7ddb60527"
check_implementation "weETH" "$WEETH" "$weeth_implementation" \
    "0xa6ca0607190d03cf16fe6f2865cf40c3d160ccf3"
check_implementation "EtherFiRedemptionManager" "$REDEMPTION_MANAGER" \
    "$redemption_manager_implementation" "0x5d53b303d62a7861f88650045b8d5deb59dfb3dc"
check_implementation "EtherFiRateLimiter" "$RATE_LIMITER" "$rate_limiter_implementation" \
    "0x9ea4d0fd09b628e23b1998f2153e27e5261b1b67"

check "totalValueInLp" "$(call "$LIQUIDITY_POOL" 'totalValueInLp()(uint128)')" \
    "$(slice "$liquidity_pool_value" 0 32)"
check "totalValueOutOfLp" "$(call "$LIQUIDITY_POOL" 'totalValueOutOfLp()(uint128)')" \
    "$(slice "$liquidity_pool_value" 32 32)"
check "totalShares" "$(call "$EETH" 'totalShares()(uint256)')" \
    "$(slice "$eeth_total_shares" 0 64)"
check "shares(weETH)" "$(call "$EETH" 'shares(address)(uint256)' "$WEETH")" \
    "$(slice "$weeth_shares" 0 64)"

redemption_info=$(cast call "$REDEMPTION_MANAGER" \
    'tokenToRedemptionInfo(address)(uint64,uint64,uint64,uint64,uint16,uint16,uint16)' \
    "$ETH_SENTINEL" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" | awk '{print $1}')
read -r rm_capacity rm_remaining rm_last_refill rm_refill_rate rm_split rm_fee rm_watermark \
    <<<"$(echo "$redemption_info" | tr '\n' ' ')"
check "redemption capacity" "$rm_capacity" "$(slice "$eth_redemption_limit" 48 16)"
check "redemption remaining" "$rm_remaining" "$(slice "$eth_redemption_limit" 32 16)"
check "redemption lastRefill" "$rm_last_refill" "$(slice "$eth_redemption_limit" 16 16)"
check "redemption refillRate" "$rm_refill_rate" "$(slice "$eth_redemption_limit" 0 16)"
check "exitFeeSplitToTreasuryInBps" "$rm_split" "$(slice "$eth_redemption_info" 60 4)"
check "exitFeeInBps" "$rm_fee" "$(slice "$eth_redemption_info" 56 4)"
check "lowWatermarkInBpsOfTvl" "$rm_watermark" "$(slice "$eth_redemption_info" 52 4)"

check_bucket() {
    local name="$1" id="$2" word="$3" limit
    limit=$(cast call "$RATE_LIMITER" 'getLimit(bytes32)(uint64,uint64,uint64,uint256)' "$id" \
        --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" | awk '{print $1}')
    local capacity remaining refill_rate last_refill
    read -r capacity remaining refill_rate last_refill <<<"$(echo "$limit" | tr '\n' ' ')"
    check "$name capacity" "$capacity" "$(slice "$word" 48 16)"
    check "$name remaining" "$remaining" "$(slice "$word" 32 16)"
    check "$name lastRefill" "$last_refill" "$(slice "$word" 16 16)"
    check "$name refillRate" "$refill_rate" "$(slice "$word" 0 16)"
}

check_bucket "mint bucket" "$(cast keccak 'EETH_MINT_LIMIT_ID')" "$eeth_mint_limit"
check_bucket "burn bucket" "$(cast keccak 'EETH_BURN_LIMIT_ID')" "$eeth_burn_limit"

if ((FAILURES > 0)); then
    echo "$FAILURES checks failed; the snapshot was not printed." >&2
    exit 1
fi
echo "All tracked slots agree with the chain." >&2

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
  "eeth_burn_limit": "$eeth_burn_limit",
  "implementations": {
    "liquidity_pool": "$liquidity_pool_implementation",
    "eeth": "$eeth_implementation",
    "weeth": "$weeth_implementation",
    "redemption_manager": "$redemption_manager_implementation",
    "rate_limiter": "$rate_limiter_implementation"
  }
}
EOF
