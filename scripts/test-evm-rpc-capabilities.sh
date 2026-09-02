#!/usr/bin/env bash
#
# Test RPC node capabilities required by tycho-indexer.
# Chain-agnostic: auto-discovers a historical block and uses Multicall3
# (deployed on virtually every EVM chain at the same address).
#
# Usage:
#   RPC_URL=https://... ./scripts/test-rpc-capabilities.sh
#   RPC_URL=https://... ./scripts/test-rpc-capabilities.sh --trace-only
#   RPC_URL=https://... ./scripts/test-rpc-capabilities.sh --main-only
#
# Optional env vars:
#   BLOCK_OFFSET  — how many blocks back from latest to use as historical (default: 100000)
#   TEST_CONTRACT — override the contract address used for tests (default: Multicall3)
#
# Exit codes:
#   0 - all tests passed
#   1 - one or more tests failed
#
# ==========================================================================
# RPC Node Requirements for tycho-indexer
# ==========================================================================
#
# The node MUST be an archive node (full historical state access).
#
# --- RPC_URL (main RPC) ---
#
# Standard methods:
#   eth_blockNumber                         — current chain tip
#   eth_gasPrice                            — legacy gas price
#   eth_maxPriorityFeePerGas                — EIP-1559 priority fee
#   eth_getBlockByNumber                    — block header at any height
#   eth_getBalance                          — native balance at any block number
#   eth_getCode                             — contract bytecode at any block number
#   eth_getStorageAt                        — single storage slot at any block number
#
# eth_call (all must work on latest, historical block number, AND block hash):
#   eth_call                                — basic contract calls
#   eth_call + state overrides              — code, balance, state, stateDiff
#                                             (slot detector, Euler Hooks DCI lens deployment)
#
# Debug / trace methods:
#   debug_storageRangeAt                    — full storage dump at a block hash
#   trace_callMany                          — token quality analysis (Parity trace API)
#
# Batching:
#   JSON-RPC batch requests                 — used extensively for parallelism
#
# --- TRACE_RPC_URL (DCI tracer, falls back to RPC_URL if unset) ---
#
# Entry point tracing (all at historical block hash):
#   eth_createAccessList                    — discover touched contracts/slots
#   eth_createAccessList + state overrides  — code, balance, stateDiff
#   debug_traceCall (prestateTracer)        — pre-state diffs
#   debug_traceCall + stateOverrides        — code, balance, stateDiff
#
# Batched pairs (sent as single batch request):
#   eth_createAccessList + debug_traceCall  — DCI trace_and_access_list
#   debug_traceCall + eth_call              — slot detector trace
#   eth_call + stateDiff (x N)             — slot detector validation
#
# ==========================================================================

set -uo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
: "${RPC_URL:?RPC_URL environment variable must be set}"

BLOCK_OFFSET="${BLOCK_OFFSET:-100000}"

# Multicall3: deployed at the same address on virtually every EVM chain
# See https://www.multicall3.com/deployments
DEFAULT_CONTRACT="0xcA11bde05977b3631167028862bE2a173976CA11"
CONTRACT="${TEST_CONTRACT:-$DEFAULT_CONTRACT}"

# Multicall3 function selectors (work on any chain)
GET_BLOCK_NUMBER_SIG="0x42cbb15c"  # getBlockNumber() → uint256
GET_BASEFEE_SIG="0x3e64a696"       # getBasefee() → uint256

# Arbitrary address padded to 32 bytes (for calldata args)
PADDED_ADDR="000000000000000000000000cA11bde05977b3631167028862bE2a173976CA11"

# Minimal EVM bytecode (returns 32 zero bytes) for code override tests
MINIMAL_BYTECODE="0x60006000526020600060003960206000f3"

# Arbitrary address for code-override tests (never deployed on any chain)
LENS_ADDR="0x0000000000000000000000000000000000001337"

# Slot zero (used for storage override tests)
SLOT_ZERO="0x0000000000000000000000000000000000000000000000000000000000000000"
OVERRIDE_VAL="0x00000000000000000000000000000000000000000000000000000000deadbeef"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
PASS=0
FAIL=0
SKIP=0
MODE="all"

for arg in "$@"; do
    case "$arg" in
        --trace-only) MODE="trace" ;;
        --main-only)  MODE="main" ;;
    esac
done

rpc_call() {
    local method="$1"
    local params="$2"
    curl -s -X POST "$RPC_URL" \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" \
        2>/dev/null || echo '{"error":"curl request failed"}'
}

rpc_batch() {
    local body="$1"
    curl -s -X POST "$RPC_URL" \
        -H "Content-Type: application/json" \
        -d "$body" \
        2>/dev/null || echo '[]'
}

has_result() {
    echo "$1" | jq -e '.result != null' >/dev/null 2>&1
}

has_error() {
    echo "$1" | jq -e '.error != null' >/dev/null 2>&1
}

check() {
    local name="$1"
    local resp="$2"
    if has_result "$resp" && ! has_error "$resp"; then
        printf "  %-65s \033[32mPASS\033[0m\n" "$name"
        PASS=$((PASS + 1))
    else
        local err
        err=$(echo "$resp" | jq -r '.error // empty' 2>/dev/null || echo "$resp")
        printf "  %-65s \033[31mFAIL\033[0m\n" "$name"
        printf "    -> %s\n" "$err"
        FAIL=$((FAIL + 1))
    fi
}

check_batch() {
    local name="$1"
    local resp="$2"
    local count="$3"

    local actual
    actual=$(echo "$resp" | jq 'length' 2>/dev/null || echo "0")
    local all_ok
    all_ok=$(echo "$resp" | jq "[.[] | select(.result != null)] | length" 2>/dev/null || echo "0")

    if [ "$actual" -ge "$count" ] && [ "$all_ok" -ge "$count" ]; then
        printf "  %-65s \033[32mPASS\033[0m\n" "$name"
        PASS=$((PASS + 1))
    else
        local errs
        errs=$(echo "$resp" | jq '[.[] | select(.error != null) | .error]' 2>/dev/null || echo "$resp")
        printf "  %-65s \033[31mFAIL\033[0m\n" "$name"
        printf "    -> got %s/%s ok, errors: %s\n" "$all_ok" "$count" "$errs"
        FAIL=$((FAIL + 1))
    fi
}

section() {
    echo ""
    echo "=== $1 ==="
}

# ---------------------------------------------------------------------------
# Discover chain info and historical block
# ---------------------------------------------------------------------------
echo "Target RPC: $RPC_URL"
echo "Test contract: $CONTRACT"
echo ""

echo "Fetching latest block..."
LATEST_RESP=$(rpc_call "eth_blockNumber" "[]")
LATEST_HEX=$(echo "$LATEST_RESP" | jq -r '.result')
LATEST_DEC=$(printf "%d" "$LATEST_HEX")
echo "Latest block: $LATEST_HEX ($LATEST_DEC)"

HIST_DEC=$((LATEST_DEC - BLOCK_OFFSET))
HIST_HEX=$(printf "0x%x" "$HIST_DEC")
echo "Historical block (latest - $BLOCK_OFFSET): $HIST_HEX ($HIST_DEC)"

echo "Fetching historical block hash..."
BLOCK_RESP=$(rpc_call "eth_getBlockByNumber" "[\"$HIST_HEX\", false]")
HIST_HASH=$(echo "$BLOCK_RESP" | jq -r '.result.hash')
if [ -z "$HIST_HASH" ] || [ "$HIST_HASH" = "null" ]; then
    echo "ERROR: Could not fetch block $HIST_HEX — is this an archive node?"
    exit 1
fi
echo "Historical block hash: $HIST_HASH"

# Simple call data used across tests: Multicall3.getBlockNumber() (no args)
CALL_DATA="$GET_BLOCK_NUMBER_SIG"

echo ""

# ===================================================================
# RPC_URL requirements (main RPC client)
# ===================================================================
if [ "$MODE" = "all" ] || [ "$MODE" = "main" ]; then

section "Standard JSON-RPC (eth_*)"

R=$(rpc_call "eth_blockNumber" "[]")
check "eth_blockNumber" "$R"

R=$(rpc_call "eth_gasPrice" "[]")
check "eth_gasPrice" "$R"

R=$(rpc_call "eth_maxPriorityFeePerGas" "[]")
check "eth_maxPriorityFeePerGas" "$R"

# --- Historical block data ---
section "Historical block data (archive node)"

R=$(rpc_call "eth_getBlockByNumber" "[\"$HIST_HEX\", false]")
check "eth_getBlockByNumber (historical)" "$R"

R=$(rpc_call "eth_getBalance" "[\"$CONTRACT\", \"$HIST_HEX\"]")
check "eth_getBalance (historical)" "$R"

R=$(rpc_call "eth_getCode" "[\"$CONTRACT\", \"$HIST_HEX\"]")
check "eth_getCode (historical)" "$R"

R=$(rpc_call "eth_getStorageAt" "[\"$CONTRACT\", \"0x0\", \"$HIST_HEX\"]")
check "eth_getStorageAt (historical)" "$R"

# --- eth_call ---
section "eth_call"

R=$(rpc_call "eth_call" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"latest\"]")
check "eth_call (latest)" "$R"

R=$(rpc_call "eth_call" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"$HIST_HEX\"]")
check "eth_call (historical block number)" "$R"

R=$(rpc_call "eth_call" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"$HIST_HASH\"]")
check "eth_call (historical block hash)" "$R"

# --- eth_call with state overrides ---
section "eth_call with state overrides"

# All override types in one call at historical block hash:
# - stateDiff on existing contract (SlotDetector)
# - code + state on fresh address (Euler Hooks DCI lens deployment)
# - balance override (entrypoint tracer)
R=$(rpc_call "eth_call" "[{\"to\":\"$LENS_ADDR\",\"data\":\"0x\"}, \"$HIST_HASH\", {\"$LENS_ADDR\":{\"code\":\"$MINIMAL_BYTECODE\",\"balance\":\"0xDE0B6B3A7640000\",\"state\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}},\"$CONTRACT\":{\"stateDiff\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}}}]")
check "eth_call + overrides: code/balance/state/stateDiff (hist hash)" "$R"

# Same at historical block number (Euler Hooks DCI uses block number, not hash)
R=$(rpc_call "eth_call" "[{\"to\":\"$LENS_ADDR\",\"data\":\"0x\"}, \"$HIST_HEX\", {\"$LENS_ADDR\":{\"code\":\"$MINIMAL_BYTECODE\",\"balance\":\"0xDE0B6B3A7640000\",\"state\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}},\"$CONTRACT\":{\"stateDiff\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}}}]")
check "eth_call + overrides: code/balance/state/stateDiff (hist num)" "$R"

# --- debug_storageRangeAt ---
section "debug_storageRangeAt"

R=$(rpc_call "debug_storageRangeAt" "[\"$HIST_HASH\", 0, \"$CONTRACT\", \"$SLOT_ZERO\", 10]")
check "debug_storageRangeAt (historical block hash)" "$R"

# --- trace_callMany ---
section "trace_callMany"

R=$(rpc_call "trace_callMany" "[[[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"},[\"trace\"]]],\"latest\"]")
check "trace_callMany (latest)" "$R"

R=$(rpc_call "trace_callMany" "[[[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"},[\"trace\"]]],\"$HIST_HEX\"]")
check "trace_callMany (historical)" "$R"

# --- JSON-RPC batching ---
section "JSON-RPC batching"

BATCH_BODY='[
  {"jsonrpc":"2.0","id":1,"method":"eth_getCode","params":["'"$CONTRACT"'","'"$HIST_HEX"'"]},
  {"jsonrpc":"2.0","id":2,"method":"eth_getBalance","params":["'"$CONTRACT"'","'"$HIST_HEX"'"]}
]'
R=$(rpc_batch "$BATCH_BODY")
check_batch "JSON-RPC batch (eth_getCode + eth_getBalance)" "$R" 2

BATCH_BODY='[
  {"jsonrpc":"2.0","id":1,"method":"eth_getStorageAt","params":["'"$CONTRACT"'","0x0","'"$HIST_HEX"'"]},
  {"jsonrpc":"2.0","id":2,"method":"eth_getStorageAt","params":["'"$CONTRACT"'","0x1","'"$HIST_HEX"'"]}
]'
R=$(rpc_batch "$BATCH_BODY")
check_batch "JSON-RPC batch (eth_getStorageAt x2)" "$R" 2

fi  # end main-only / all

# ===================================================================
# TRACE_RPC_URL requirements (DCI tracer + service tracer)
# Also used by RPC_URL for the service-side tracer and slot detector.
# ===================================================================
if [ "$MODE" = "all" ] || [ "$MODE" = "trace" ]; then

section "eth_createAccessList"

R=$(rpc_call "eth_createAccessList" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"$HIST_HASH\"]")
check "eth_createAccessList (historical block hash)" "$R"

R=$(rpc_call "eth_createAccessList" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"$HIST_HASH\", {\"$CONTRACT\":{\"code\":\"$MINIMAL_BYTECODE\",\"balance\":\"0xDE0B6B3A7640000\",\"stateDiff\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}}}]")
check "eth_createAccessList + overrides (historical)" "$R"

section "debug_traceCall (prestateTracer)"

R=$(rpc_call "debug_traceCall" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"$HIST_HASH\", {\"tracer\":\"prestateTracer\",\"enableReturnData\":true}]")
check "debug_traceCall prestateTracer (historical block hash)" "$R"

R=$(rpc_call "debug_traceCall" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"$HIST_HASH\", {\"tracer\":\"prestateTracer\",\"enableReturnData\":true,\"stateOverrides\":{\"$CONTRACT\":{\"code\":\"$MINIMAL_BYTECODE\",\"balance\":\"0xDE0B6B3A7640000\",\"stateDiff\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}}}}]")
check "debug_traceCall prestateTracer + overrides (historical)" "$R"

section "Batched trace_and_access_list (eth_createAccessList + debug_traceCall)"

BATCH_BODY='[
  {"jsonrpc":"2.0","id":1,"method":"eth_createAccessList","params":[{"to":"'"$CONTRACT"'","data":"'"$CALL_DATA"'"},"'"$HIST_HASH"'"]},
  {"jsonrpc":"2.0","id":2,"method":"debug_traceCall","params":[{"to":"'"$CONTRACT"'","data":"'"$CALL_DATA"'"},"'"$HIST_HASH"'",{"tracer":"prestateTracer","enableReturnData":true}]}
]'
R=$(rpc_batch "$BATCH_BODY")
check_batch "Batch: eth_createAccessList + debug_traceCall" "$R" 2

section "Batched slot_detector_trace (debug_traceCall + eth_call)"

BATCH_BODY='[
  {"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":"'"$CONTRACT"'","data":"'"$CALL_DATA"'"},"'"$HIST_HASH"'",{"tracer":"prestateTracer","enableReturnData":true}]},
  {"jsonrpc":"2.0","id":2,"method":"eth_call","params":[{"to":"'"$CONTRACT"'","data":"'"$CALL_DATA"'"},"'"$HIST_HASH"'"]}
]'
R=$(rpc_batch "$BATCH_BODY")
check_batch "Batch: debug_traceCall + eth_call (slot detector)" "$R" 2

section "Batched slot_detector_tests (eth_call + stateDiff)"

BATCH_BODY='[
  {"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{"to":"'"$CONTRACT"'","data":"'"$CALL_DATA"'"},"'"$HIST_HASH"'",{"'"$CONTRACT"'":{"stateDiff":{"'"$SLOT_ZERO"'":"'"$OVERRIDE_VAL"'"}}}]},
  {"jsonrpc":"2.0","id":2,"method":"eth_call","params":[{"to":"'"$CONTRACT"'","data":"'"$CALL_DATA"'"},"'"$HIST_HASH"'",{"'"$CONTRACT"'":{"stateDiff":{"'"$SLOT_ZERO"'":"'"$OVERRIDE_VAL"'"}}}]}
]'
R=$(rpc_batch "$BATCH_BODY")
check_batch "Batch: eth_call + stateDiff override (slot detector tests)" "$R" 2

fi  # end trace-only / all

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "==========================================="
printf "Results: \033[32m%d passed\033[0m, \033[31m%d failed\033[0m, \033[33m%d skipped\033[0m\n" "$PASS" "$FAIL" "$SKIP"
echo "==========================================="

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "Some RPC methods are not supported. Review failures above."
    exit 1
fi
