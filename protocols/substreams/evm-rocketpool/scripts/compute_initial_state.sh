#!/bin/bash
# Script to compute initial state values for ethereum-rocketpool.yaml params
#
# This script reads the on-chain state at a specific block to generate the initial
# attribute values needed when starting the substream from a specific block.
#
# Usage: RPC_URL=<your-rpc-url> ./compute_initial_state.sh [block_number]
#
# The block number defaults to 24479994 (v1.4 upgrade block).

set -e

# Default to the v1.4 upgrade block
BLOCK_NUMBER=${1:-24479994}

if [ -z "$RPC_URL" ]; then
  echo "Error: RPC_URL environment variable is required"
  exit 1
fi

echo "Computing initial state at block $BLOCK_NUMBER..."
echo ""

# Contract addresses (from constants.rs)
ROCKET_VAULT="0x3bDC69C4E5e13E52A65f5583c23EFB9636b469d6"
ROCKET_STORAGE="0x1d8f8f00cfa6758d7bE78336684788Fb0ee0Fa46"
RETH_ADDRESS="0xae78736Cd615f374D3085123A210448E74Fc6393"
ROCKET_NETWORK_BALANCES="0x1D9F14C6Bfd8358b589964baD8665AdD248E9473"

# Storage slots (from constants.rs)
DEPOSIT_POOL_ETH_BALANCE_SLOT="0x00ab4654686e0d7a1f921cc85a932fd8efbc8a1f247b51fa6bca2f7a3976a5bb"
DEPOSITS_ENABLED_SLOT="0x7bd5d699fdfcd0cf7b26d3fc339f1567cecb978e8ce24b7b6ed7d192e1bbb663"
DEPOSIT_ASSIGN_ENABLED_SLOT="0x3c4ef260cb76105ef0fda3d75cf7af776accf2a871c39fd5530453efa532aba4"
DEPOSIT_ASSIGN_MAXIMUM_SLOT="0xa2574dbdd30c823af5a27800f3329b5f8f5fa1e4cb116c254794974425497fb3"
DEPOSIT_ASSIGN_SOCIALISED_MAXIMUM_SLOT="0xd6794381ca0356c0f5fabe729b1ea706b25013e48d1d1bb2441c2bd5053a975a"
MIN_DEPOSIT_AMOUNT_SLOT="0xba4dab8f9b8f22679cf8c926f5bd528d08a526cbe2bb39d1b1f1566d0d30ad0c"
MAX_DEPOSIT_POOL_SIZE_SLOT="0xefeb8d9f341f931c14ed8c1156bdb235390b183f1b94f522d4d72c5d24779598"
DEPOSIT_FEE_SLOT="0xa1713e68e8e6d7580de48bb14bd78c7f293a5a0e42a40f7fe428d9943dc63264"
MEGAPOOL_QUEUE_REQUESTED_TOTAL_SLOT="0x70acbb59da22199e2dc0759d60b0224ec935b6c5c70975c698025712f413ccdd"
TARGET_RETH_COLLATERAL_RATE_SLOT="0xe1cd6c7fac18bc41fcd8660dbc3a1370373485f93fbccc910651118840f7c3a8"

# Helper function to read storage
read_storage() {
  local contract=$1
  local slot=$2
  cast storage "$contract" "$slot" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" 2>/dev/null
}

# Helper function to call a contract method
call_method() {
  local contract=$1
  local signature=$2
  cast call "$contract" "$signature" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" 2>/dev/null
}

# Helper function to convert decimal to 32-byte padded hex
to_padded_hex() {
  local dec_value=$1
  local hex_value=$(cast to-hex "$dec_value" 2>/dev/null | sed 's/0x//')
  # Pad to 64 hex characters (32 bytes)
  printf "0x%064s" "$hex_value" | tr ' ' '0'
}

# Read storage values from RocketVault
echo "Reading from RocketVault ($ROCKET_VAULT)..."
deposit_contract_balance=$(read_storage "$ROCKET_VAULT" "$DEPOSIT_POOL_ETH_BALANCE_SLOT")

# Read storage values from RocketStorage (settings)
echo "Reading from RocketStorage ($ROCKET_STORAGE)..."
deposits_enabled=$(read_storage "$ROCKET_STORAGE" "$DEPOSITS_ENABLED_SLOT")
deposit_assigning_enabled=$(read_storage "$ROCKET_STORAGE" "$DEPOSIT_ASSIGN_ENABLED_SLOT")
deposit_assign_maximum=$(read_storage "$ROCKET_STORAGE" "$DEPOSIT_ASSIGN_MAXIMUM_SLOT")
deposit_assign_socialised_maximum=$(read_storage "$ROCKET_STORAGE" "$DEPOSIT_ASSIGN_SOCIALISED_MAXIMUM_SLOT")
min_deposit_amount=$(read_storage "$ROCKET_STORAGE" "$MIN_DEPOSIT_AMOUNT_SLOT")
max_deposit_pool_size=$(read_storage "$ROCKET_STORAGE" "$MAX_DEPOSIT_POOL_SIZE_SLOT")
deposit_fee=$(read_storage "$ROCKET_STORAGE" "$DEPOSIT_FEE_SLOT")
megapool_queue_requested_total=$(read_storage "$ROCKET_STORAGE" "$MEGAPOOL_QUEUE_REQUESTED_TOTAL_SLOT")
target_reth_collateral_rate=$(read_storage "$ROCKET_STORAGE" "$TARGET_RETH_COLLATERAL_RATE_SLOT")

# Get rETH contract ETH balance
echo "Reading rETH contract balance..."
reth_balance_wei=$(cast balance "$RETH_ADDRESS" --block "$BLOCK_NUMBER" --rpc-url "$RPC_URL" 2>/dev/null | cut -d' ' -f1)
reth_contract_liquidity=$(to_padded_hex "$reth_balance_wei")

# Call RocketNetworkBalances for total_eth and reth_supply
echo "Calling RocketNetworkBalances ($ROCKET_NETWORK_BALANCES)..."
total_eth_dec=$(call_method "$ROCKET_NETWORK_BALANCES" "getTotalETHBalance()(uint256)" | cut -d' ' -f1)
total_eth=$(to_padded_hex "$total_eth_dec")

reth_supply_dec=$(call_method "$ROCKET_NETWORK_BALANCES" "getTotalRETHSupply()(uint256)" | cut -d' ' -f1)
reth_supply=$(to_padded_hex "$reth_supply_dec")

# Output the JSON for ethereum-rocketpool.yaml
echo ""
echo "=== Initial State JSON for ethereum-rocketpool.yaml ==="
echo ""
cat <<EOF
map_protocol_changes: |
    {
      "deposit_contract_balance": "$deposit_contract_balance",
      "reth_contract_liquidity": "$reth_contract_liquidity",
      "deposits_enabled": "$deposits_enabled",
      "deposit_assigning_enabled": "$deposit_assigning_enabled",
      "deposit_assign_maximum": "$deposit_assign_maximum",
      "deposit_assign_socialised_maximum": "$deposit_assign_socialised_maximum",
      "min_deposit_amount": "$min_deposit_amount",
      "max_deposit_pool_size": "$max_deposit_pool_size",
      "deposit_fee": "$deposit_fee",
      "megapool_queue_requested_total": "$megapool_queue_requested_total",
      "target_reth_collateral_rate": "$target_reth_collateral_rate",
      "total_eth": "$total_eth",
      "reth_supply": "$reth_supply"
    }
EOF
