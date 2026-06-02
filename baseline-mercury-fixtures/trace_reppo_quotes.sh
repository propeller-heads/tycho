#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${BASE_RPC_URL:-}" ]]; then
  echo "BASE_RPC_URL is required" >&2
  exit 1
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="$ROOT_DIR/reppo"
mkdir -p "$OUT_DIR"

RELAY="0xc81Fd894C0acE037d133aF4886550aC8133568E8"
BTOKEN="0xFf8104251E7761163faC3211eF5583FB3F8583d6"
RESERVE="0x0b3e328455c4059EEb9e3f84b5543F74E24e7E1b"
CALLER="0x0000000000000000000000000000000000000001"
AMOUNT="1000000000000000000"

if [[ -n "${BLOCK_NUMBER:-}" ]]; then
  BLOCK="$BLOCK_NUMBER"
else
  # Use latest - 3 to avoid racing against a just-produced block.
  LATEST="$(cast block-number --rpc-url "$BASE_RPC_URL")"
  BLOCK="$((LATEST - 3))"
fi

BUY_DATA="$(cast calldata 'quoteBuyExactIn(address,uint256)' "$BTOKEN" "$AMOUNT")"
SELL_DATA="$(cast calldata 'quoteSellExactIn(address,uint256)' "$BTOKEN" "$AMOUNT")"

trace_call() {
  local data="$1"
  local out="$2"

  cast rpc debug_traceCall \
    "{\"from\":\"$CALLER\",\"to\":\"$RELAY\",\"data\":\"$data\",\"gas\":\"0x989680\",\"maxFeePerGas\":\"0x3b9aca00\",\"maxPriorityFeePerGas\":\"0x1\"}" \
    "0x$(printf '%x' "$BLOCK")" \
    '{"tracer":"prestateTracer","tracerConfig":{"diffMode":false},"timeout":"20s"}' \
    --rpc-url "$BASE_RPC_URL" | jq --sort-keys . > "$out"
}

call_quote() {
  local sig="$1"
  local out="$2"

  cast call "$RELAY" "$sig" "$BTOKEN" "$AMOUNT" \
    --block "$BLOCK" \
    --rpc-url "$BASE_RPC_URL" > "$out"
}

trace_call "$BUY_DATA" "$OUT_DIR/quote_buy_exact_in_prestate.json"
trace_call "$SELL_DATA" "$OUT_DIR/quote_sell_exact_in_prestate.json"

call_quote 'quoteBuyExactIn(address,uint256)(uint256,uint256,uint256)' \
  "$OUT_DIR/quote_buy_exact_in.out"
call_quote 'quoteSellExactIn(address,uint256)(uint256,uint256,uint256)' \
  "$OUT_DIR/quote_sell_exact_in.out"

cat > "$OUT_DIR/fixture.json" <<JSON
{
  "chain": "base",
  "block_number": $BLOCK,
  "relay": "$RELAY",
  "btoken": "$BTOKEN",
  "reserve": "$RESERVE",
  "caller": "$CALLER",
  "amount": "$AMOUNT",
  "quote_buy_exact_in": $(jq -Rs 'split("\n") | map(select(length > 0))' "$OUT_DIR/quote_buy_exact_in.out"),
  "quote_sell_exact_in": $(jq -Rs 'split("\n") | map(select(length > 0))' "$OUT_DIR/quote_sell_exact_in.out"),
  "prestate_files": {
    "quote_buy_exact_in": "quote_buy_exact_in_prestate.json",
    "quote_sell_exact_in": "quote_sell_exact_in_prestate.json"
  }
}
JSON

jq --sort-keys . "$OUT_DIR/fixture.json" > "$OUT_DIR/fixture.tmp"
mv "$OUT_DIR/fixture.tmp" "$OUT_DIR/fixture.json"

echo "Wrote REPPO quote fixture to $OUT_DIR"
echo "Pinned block: $BLOCK"
