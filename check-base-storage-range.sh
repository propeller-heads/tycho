#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <base-rpc-url>" >&2
  exit 1
fi

RPC_URL="$1"

BLOCK_NUMBER_DEC="46596026"
BLOCK_NUMBER_HEX="0x2c6fb7a"
BLOCK_HASH="0x53bdf702bceca9482075da6fd0e68a204b9886f0f42a06fb03a3fd3762ea6d36"
BASELINE_RELAY="0xc81Fd894C0acE037d133aF4886550aC8133568E8"
BASE_RESERVE_TOKEN="0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b"
START_KEY="0x0000000000000000000000000000000000000000000000000000000000000000"

rpc() {
  curl -sS "$RPC_URL" \
    -H "Content-Type: application/json" \
    --data "$1"
}

print_json() {
  local body
  body="$(cat)"

  if command -v jq >/dev/null 2>&1; then
    if jq -e . >/dev/null 2>&1 <<<"$body"; then
      jq . <<<"$body"
    else
      printf '%s\n' "$body"
    fi
  else
    printf '%s\n' "$body"
  fi
}

echo "== web3_clientVersion =="
rpc '{"jsonrpc":"2.0","id":1,"method":"web3_clientVersion","params":[]}' | print_json

echo
echo "== eth_getBlockByNumber sanity check =="
echo "Block: ${BLOCK_NUMBER_DEC} (${BLOCK_NUMBER_HEX})"
rpc "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"eth_getBlockByNumber\",\"params\":[\"${BLOCK_NUMBER_HEX}\",false]}" | print_json

echo
echo "== debug_storageRangeAt: historical Baseline relay =="
echo "Block hash: ${BLOCK_HASH}"
echo "Tx index: 0"
echo "Address: ${BASELINE_RELAY}"
echo "Start key: ${START_KEY}"
echo "Max results: 1"
rpc "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"debug_storageRangeAt\",\"params\":[\"${BLOCK_HASH}\",0,\"${BASELINE_RELAY}\",\"${START_KEY}\",1]}" | print_json

echo
echo "== debug_storageRangeAt: latest Base reserve token sanity check =="
LATEST_BODY="$(rpc '{"jsonrpc":"2.0","id":4,"method":"eth_getBlockByNumber","params":["latest",false]}')"
LATEST_HASH="$(jq -r '.result.hash // empty' <<<"$LATEST_BODY" 2>/dev/null || true)"

if [[ -z "$LATEST_HASH" ]]; then
  echo "Could not resolve latest block hash; skipping latest storage range check." >&2
  printf '%s\n' "$LATEST_BODY" | print_json
  exit 0
fi

echo "Latest block hash: ${LATEST_HASH}"
echo "Address: ${BASE_RESERVE_TOKEN}"
rpc "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"debug_storageRangeAt\",\"params\":[\"${LATEST_HASH}\",0,\"${BASE_RESERVE_TOKEN}\",\"${START_KEY}\",1]}" | print_json
