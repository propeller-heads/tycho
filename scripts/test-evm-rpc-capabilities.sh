#!/usr/bin/env bash
#
# Test RPC node capabilities required by tycho-indexer.
# Chain-agnostic: auto-discovers a historical block and uses Multicall3
# (deployed on virtually every EVM chain at the same address).
#
# Usage:
#   RPC_URL=https://... ./scripts/test-evm-rpc-capabilities.sh
#   RPC_URL=https://... TRACE_RPC_URL=https://... ./scripts/test-evm-rpc-capabilities.sh
#   RPC_URL=https://... ./scripts/test-evm-rpc-capabilities.sh --core-only
#   RPC_URL=https://... TRACE_RPC_URL=https://... ./scripts/test-evm-rpc-capabilities.sh --dci-only
#
# Optional env vars:
#   TRACE_RPC_URL   — DCI tracer endpoint (default: RPC_URL, same as the indexer)
#   BLOCK_OFFSET    — how many blocks back from latest to use as historical (default: 100000)
#   TEST_CONTRACT   — override the contract address used for tests (default: Multicall3)
#   MAX_BATCH_SIZE  — requests per batch (default: 50, the indexer's default)
#   STORAGE_BATCH_SIZE — eth_getStorageAt requests per batch (default: 1000, indexer's default)
#
# Exit codes:
#   0 - all required capabilities present (warnings allowed)
#   1 - one or more required capabilities missing
#
# ==========================================================================
# RPC Node Requirements for tycho-indexer
# ==========================================================================
#
# The node MUST be an archive node (full historical state access).
#
# Requirements split by feature, not by endpoint: core indexing always runs against RPC_URL,
# while DCI needs capabilities on *both* RPC_URL and TRACE_RPC_URL (see the DCI section).
#
# --- Core indexing — RPC_URL (crates/tycho-ethereum/src/rpc/mod.rs) ---
#
# Standard methods:
#   eth_blockNumber                         — current chain tip
#   eth_getBlockByNumber                    — block header at any height (and `latest`)
#   eth_getBalance                          — native balance at any block number
#   eth_getCode                             — contract bytecode at any block number
#   eth_getStorageAt                        — single storage slot at any block number
#                                             (AccountExtractor, selected slots)
#   debug_storageRangeAt                    — full storage dump at a block hash, 100k entries per
#                                             page. Needed by `initialized_accounts` in
#                                             extractors.yaml and by DCI account snapshots
#                                             (AccountExtractor)
#
# eth_call (on `latest` AND on a historical block number):
#   eth_call                                — token symbol/decimals, DCI metadata calls
#   eth_call + state overrides              — code, balance, state, stateDiff, 30M gas limit.
#                                             EthCallDetector injects the Analyzer and Forwarder
#                                             bytecode to grade every unknown token
#
# Batching (defaults from RPCBatchingConfig::enabled_with_defaults):
#   50 requests per batch, 1000 for eth_getStorageAt
#     eth_getCode + eth_getBalance          — AccountExtractor
#     eth_getStorageAt x N                  — AccountExtractor
#
# Optional:
#   eth_gasPrice / eth_maxPriorityFeePerGas — FeePriceGetter, for downstream consumers. Only one
#                                             is needed; get_gas_price falls back to eth_gasPrice.
#
# --- DCI — RPC_URL (crates/tycho-ethereum/src/services/entrypoint_tracer/) ---
#
# The Uniswap V4 hooks DCI builds its slot detectors on the *main* client, so these are
# required on RPC_URL even when TRACE_RPC_URL points elsewhere
# (extractor/dynamic_contract_indexer/hooks/hooks_dci_builder.rs).
#
#   debug_traceCall (prestateTracer)        — BalanceSlotDetector / AllowanceSlotDetector
#   debug_traceCall + eth_call in one batch — slot detector trace (needs batch >= 2)
#   eth_call + stateDiff x N in one batch   — slot detector tests
#
# --- DCI — TRACE_RPC_URL (entrypoint tracer, falls back to RPC_URL if unset) ---
#
# All at a historical block hash (entrypoint_tracer/tracer.rs):
#   eth_createAccessList                    — discover touched contracts/slots
#   eth_createAccessList + state overrides  — code, balance, stateDiff
#   debug_traceCall (prestateTracer)        — pre-state diffs
#   debug_traceCall + stateOverrides        — code, balance, stateDiff
#   both in one batch                       — trace_and_access_list requires batching
#
# ==========================================================================

# `-e` is deliberately omitted: a failing capability check must not abort the run.
set -uo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
: "${RPC_URL:?RPC_URL environment variable must be set}"

# The indexer falls back to RPC_URL when TRACE_RPC_URL is unset (extractor/factory.rs).
TRACE_URL="${TRACE_RPC_URL:-$RPC_URL}"

BLOCK_OFFSET="${BLOCK_OFFSET:-100000}"

# Mirrors RPCBatchingConfig::enabled_with_defaults()
MAX_BATCH_SIZE="${MAX_BATCH_SIZE:-50}"
STORAGE_BATCH_SIZE="${STORAGE_BATCH_SIZE:-1000}"

# Mirrors debug_storage_range_at's page size
STORAGE_RANGE_LIMIT=100000

# Mirrors EthCallDetector::GAS_LIMIT (30M)
CALL_GAS_LIMIT="0x1c9c380"

# Multicall3: deployed at the same address on virtually every EVM chain
# See https://www.multicall3.com/deployments
DEFAULT_CONTRACT="0xcA11bde05977b3631167028862bE2a173976CA11"
CONTRACT="${TEST_CONTRACT:-$DEFAULT_CONTRACT}"

# Multicall3.getBlockNumber() → uint256; works on any chain
CALL_DATA="0x42cbb15c"

# Call object used by the trace tests. `gas` is set explicitly: without it a node
# charges the sender its whole RPC gas cap, and the default sender (0x0) fails the
# funding check before the method under test is ever exercised.
TRACE_TX="{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\",\"gas\":\"$CALL_GAS_LIMIT\"}"

# Minimal EVM bytecode (returns 32 zero bytes) for code override tests
MINIMAL_BYTECODE="0x60006000526020600060003960206000f3"

# Arbitrary address for code-override tests (never deployed on any chain).
# Same address the Euler Hooks DCI deploys its lens to via state override.
LENS_ADDR="0x0000000000000000000000000000000000001337"

# Slot zero (used for storage override tests)
SLOT_ZERO="0x0000000000000000000000000000000000000000000000000000000000000000"
OVERRIDE_VAL="0x00000000000000000000000000000000000000000000000000000000deadbeef"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
PASS=0
FAIL=0
WARN=0
MODE="all"

for arg in "$@"; do
    case "$arg" in
        --core-only) MODE="core" ;;
        --dci-only) MODE="dci" ;;
        -h | --help)
            sed -n '3,22p' "$0"
            exit 0
            ;;
        *)
            printf "Unknown argument: %s (try --help)\n" "$arg" >&2
            exit 2
            ;;
    esac
done

rpc_call() {
    local url="$1"
    local method="$2"
    local params="$3"
    curl -s -X POST "$url" \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" \
        2>/dev/null || echo '{"error":"curl request failed"}'
}

rpc_batch() {
    local url="$1"
    local body="$2"
    curl -s -X POST "$url" \
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

error_of() {
    echo "$1" | jq -rc '.error // empty' 2>/dev/null || echo "$1"
}

check() {
    local name="$1"
    local resp="$2"
    if has_result "$resp" && ! has_error "$resp"; then
        printf "  %-65s \033[32mPASS\033[0m\n" "$name"
        PASS=$((PASS + 1))
    else
        printf "  %-65s \033[31mFAIL\033[0m\n" "$name"
        printf "    -> %s\n" "$(error_of "$resp")"
        FAIL=$((FAIL + 1))
    fi
}

# Same as check(), but a missing capability is reported as a warning and does
# not fail the run. Used for methods the indexer does not depend on.
check_optional() {
    local name="$1"
    local resp="$2"
    if has_result "$resp" && ! has_error "$resp"; then
        printf "  %-65s \033[32mPASS\033[0m\n" "$name"
        PASS=$((PASS + 1))
    else
        printf "  %-65s \033[33mWARN\033[0m\n" "$name"
        printf "    -> %s\n" "$(error_of "$resp")"
        WARN=$((WARN + 1))
    fi
}

check_batch() {
    local name="$1"
    local resp="$2"
    local count="$3"

    local actual all_ok
    actual=$(echo "$resp" | jq 'length' 2>/dev/null || echo "0")
    all_ok=$(echo "$resp" | jq "[.[] | select(.result != null)] | length" 2>/dev/null || echo "0")

    if [ "$actual" -ge "$count" ] && [ "$all_ok" -ge "$count" ]; then
        printf "  %-65s \033[32mPASS\033[0m\n" "$name"
        PASS=$((PASS + 1))
    else
        local errs
        errs=$(echo "$resp" | jq -c '[.[] | select(.error != null) | .error] | unique' 2>/dev/null || echo "$resp")
        printf "  %-65s \033[31mFAIL\033[0m\n" "$name"
        printf "    -> got %s/%s ok, errors: %s\n" "$all_ok" "$count" "$errs"
        FAIL=$((FAIL + 1))
    fi
}

section() {
    echo ""
    echo "=== $1 ==="
}

# Collects newline-delimited JSON request objects from stdin into a batch array.
as_batch() {
    jq -sc '.'
}

# Alternating eth_getCode / eth_getBalance batch at the historical block number.
code_and_balance_batch() {
    local n="$1" i method
    for ((i = 0; i < n; i++)); do
        if [ $((i % 2)) -eq 0 ]; then method="eth_getCode"; else method="eth_getBalance"; fi
        printf '{"jsonrpc":"2.0","id":%d,"method":"%s","params":["%s","%s"]}\n' \
            "$((i + 1))" "$method" "$CONTRACT" "$HIST_HEX"
    done | as_batch
}

storage_slot_batch() {
    local n="$1" i
    for ((i = 0; i < n; i++)); do
        printf '{"jsonrpc":"2.0","id":%d,"method":"eth_getStorageAt","params":["%s","0x%x","%s"]}\n' \
            "$((i + 1))" "$CONTRACT" "$i" "$HIST_HEX"
    done | as_batch
}

# Batch of eth_call + stateDiff overrides, the shape slot_detector_tests sends.
state_diff_call_batch() {
    local n="$1" i
    for ((i = 0; i < n; i++)); do
        printf '{"jsonrpc":"2.0","id":%d,"method":"eth_call","params":[{"to":"%s","data":"%s"},"latest",{"%s":{"stateDiff":{"%s":"%s"}}}]}\n' \
            "$((i + 1))" "$CONTRACT" "$CALL_DATA" "$CONTRACT" "$SLOT_ZERO" "$OVERRIDE_VAL"
    done | as_batch
}

# ---------------------------------------------------------------------------
# Discover chain info and historical block
# ---------------------------------------------------------------------------
echo "Target RPC:    $RPC_URL"
echo "Trace RPC:     $TRACE_URL"
echo "Test contract: $CONTRACT"
echo ""

echo "Fetching latest block..."
LATEST_RESP=$(rpc_call "$RPC_URL" "eth_blockNumber" "[]")
LATEST_HEX=$(echo "$LATEST_RESP" | jq -r '.result // empty')
if [ -z "$LATEST_HEX" ]; then
    printf "ERROR: eth_blockNumber failed: %s\n" "$(error_of "$LATEST_RESP")"
    exit 1
fi
LATEST_DEC=$((LATEST_HEX))
echo "Latest block: $LATEST_HEX ($LATEST_DEC)"

HIST_DEC=$((LATEST_DEC - BLOCK_OFFSET))
if [ "$HIST_DEC" -lt 1 ]; then
    # Young chain: fall back to the midpoint of its history.
    HIST_DEC=$((LATEST_DEC / 2))
    [ "$HIST_DEC" -lt 1 ] && HIST_DEC=1
    echo "Chain is shorter than BLOCK_OFFSET=$BLOCK_OFFSET, using block $HIST_DEC instead"
fi
HIST_HEX=$(printf "0x%x" "$HIST_DEC")
echo "Historical block: $HIST_HEX ($HIST_DEC)"

echo "Fetching historical block hash..."
BLOCK_RESP=$(rpc_call "$RPC_URL" "eth_getBlockByNumber" "[\"$HIST_HEX\", false]")
HIST_HASH=$(echo "$BLOCK_RESP" | jq -r '.result.hash // empty')
if [ -z "$HIST_HASH" ]; then
    echo "ERROR: Could not fetch block $HIST_HEX — is this an archive node?"
    printf "    -> %s\n" "$(error_of "$BLOCK_RESP")"
    exit 1
fi
echo "Historical block hash: $HIST_HASH"

echo ""

# ===================================================================
# Core indexing (RPC_URL)
# ===================================================================
if [ "$MODE" = "all" ] || [ "$MODE" = "core" ]; then

    section "Standard JSON-RPC (eth_*)"

    R=$(rpc_call "$RPC_URL" "eth_blockNumber" "[]")
    check "eth_blockNumber" "$R"

    R=$(rpc_call "$RPC_URL" "eth_getBlockByNumber" "[\"latest\", false]")
    check "eth_getBlockByNumber (latest)" "$R"

    # get_gas_price() prefers eth_maxPriorityFeePerGas and falls back to eth_gasPrice,
    # so either one satisfies FeePriceGetter. Both are optional for the indexer itself.
    R=$(rpc_call "$RPC_URL" "eth_maxPriorityFeePerGas" "[]")
    check_optional "eth_maxPriorityFeePerGas (EIP-1559 fee price)" "$R"

    R=$(rpc_call "$RPC_URL" "eth_gasPrice" "[]")
    check_optional "eth_gasPrice (legacy fee price fallback)" "$R"

    section "Historical block data (archive node)"

    R=$(rpc_call "$RPC_URL" "eth_getBlockByNumber" "[\"$HIST_HEX\", false]")
    check "eth_getBlockByNumber (historical)" "$R"

    R=$(rpc_call "$RPC_URL" "eth_getBalance" "[\"$CONTRACT\", \"$HIST_HEX\"]")
    check "eth_getBalance (historical)" "$R"

    R=$(rpc_call "$RPC_URL" "eth_getCode" "[\"$CONTRACT\", \"$HIST_HEX\"]")
    check "eth_getCode (historical)" "$R"

    R=$(rpc_call "$RPC_URL" "eth_getStorageAt" "[\"$CONTRACT\", \"0x0\", \"$HIST_HEX\"]")
    check "eth_getStorageAt (historical)" "$R"

    section "debug_storageRangeAt (full storage dumps)"

    R=$(rpc_call "$RPC_URL" "debug_storageRangeAt" \
        "[\"$HIST_HASH\", 0, \"$CONTRACT\", \"$SLOT_ZERO\", $STORAGE_RANGE_LIMIT]")
    check "debug_storageRangeAt ($STORAGE_RANGE_LIMIT entries, hist hash)" "$R"

    section "eth_call"

    R=$(rpc_call "$RPC_URL" "eth_call" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"latest\"]")
    check "eth_call (latest)" "$R"

    R=$(rpc_call "$RPC_URL" "eth_call" "[{\"to\":\"$CONTRACT\",\"data\":\"$CALL_DATA\"}, \"$HIST_HEX\"]")
    check "eth_call (historical block number)" "$R"

    section "eth_call with state overrides"

    # All override types in one call, plus the 30M gas limit EthCallDetector uses:
    # - code + state on a fresh address (token analyzer bytecode, Euler Hooks DCI lens)
    # - balance override (entrypoint tracer)
    # - stateDiff on an existing contract (slot detector)
    OVERRIDES="{\"$LENS_ADDR\":{\"code\":\"$MINIMAL_BYTECODE\",\"balance\":\"0xde0b6b3a7640000\",\"state\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}},\"$CONTRACT\":{\"stateDiff\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}}}"
    CALL_OBJ="{\"to\":\"$LENS_ADDR\",\"data\":\"0x\",\"gas\":\"$CALL_GAS_LIMIT\"}"

    R=$(rpc_call "$RPC_URL" "eth_call" "[$CALL_OBJ, \"latest\", $OVERRIDES]")
    check "eth_call + overrides: code/balance/state/stateDiff (latest)" "$R"

    R=$(rpc_call "$RPC_URL" "eth_call" "[$CALL_OBJ, \"$HIST_HEX\", $OVERRIDES]")
    check "eth_call + overrides: code/balance/state/stateDiff (hist num)" "$R"

    section "JSON-RPC batching (indexer defaults: $MAX_BATCH_SIZE, $STORAGE_BATCH_SIZE for storage)"

    R=$(rpc_batch "$RPC_URL" "$(code_and_balance_batch "$MAX_BATCH_SIZE")")
    check_batch "Batch x$MAX_BATCH_SIZE: eth_getCode + eth_getBalance" "$R" "$MAX_BATCH_SIZE"

    R=$(rpc_batch "$RPC_URL" "$(storage_slot_batch "$STORAGE_BATCH_SIZE")")
    check_batch "Batch x$STORAGE_BATCH_SIZE: eth_getStorageAt" "$R" "$STORAGE_BATCH_SIZE"

fi # end core-only / all

# ===================================================================
# Dynamic Contract Indexing
# ===================================================================
if [ "$MODE" = "all" ] || [ "$MODE" = "dci" ]; then

    # The Uniswap V4 hooks DCI builds its slot detectors on the main client, so
    # these run against RPC_URL even when TRACE_RPC_URL points somewhere else.
    section "DCI slot detectors — main RPC"

    R=$(rpc_call "$RPC_URL" "debug_traceCall" \
        "[$TRACE_TX, \"latest\", {\"tracer\":\"prestateTracer\",\"enableReturnData\":true}]")
    check "debug_traceCall prestateTracer (latest)" "$R"

    BATCH_BODY='[
  {"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":['"$TRACE_TX"',"latest",{"tracer":"prestateTracer","enableReturnData":true}]},
  {"jsonrpc":"2.0","id":2,"method":"eth_call","params":[{"to":"'"$CONTRACT"'","data":"'"$CALL_DATA"'"},"latest"]}
]'
    R=$(rpc_batch "$RPC_URL" "$BATCH_BODY")
    check_batch "Batch: debug_traceCall + eth_call (slot detector trace)" "$R" 2

    R=$(rpc_batch "$RPC_URL" "$(state_diff_call_batch "$MAX_BATCH_SIZE")")
    check_batch "Batch x$MAX_BATCH_SIZE: eth_call + stateDiff (slot detector tests)" "$R" "$MAX_BATCH_SIZE"

    section "DCI entrypoint tracer — trace RPC"

    R=$(rpc_call "$TRACE_URL" "eth_createAccessList" \
        "[$TRACE_TX, \"$HIST_HASH\"]")
    check "eth_createAccessList (historical block hash)" "$R"

    TRACE_OVERRIDES="{\"$CONTRACT\":{\"code\":\"$MINIMAL_BYTECODE\",\"balance\":\"0xde0b6b3a7640000\",\"stateDiff\":{\"$SLOT_ZERO\":\"$OVERRIDE_VAL\"}}}"

    R=$(rpc_call "$TRACE_URL" "eth_createAccessList" \
        "[$TRACE_TX, \"$HIST_HASH\", $TRACE_OVERRIDES]")
    check "eth_createAccessList + overrides (historical)" "$R"

    R=$(rpc_call "$TRACE_URL" "debug_traceCall" \
        "[$TRACE_TX, \"$HIST_HASH\", {\"tracer\":\"prestateTracer\",\"enableReturnData\":true}]")
    check "debug_traceCall prestateTracer (historical block hash)" "$R"

    R=$(rpc_call "$TRACE_URL" "debug_traceCall" \
        "[$TRACE_TX, \"$HIST_HASH\", {\"tracer\":\"prestateTracer\",\"enableReturnData\":true,\"stateOverrides\":$TRACE_OVERRIDES}]")
    check "debug_traceCall prestateTracer + overrides (historical)" "$R"

    BATCH_BODY='[
  {"jsonrpc":"2.0","id":1,"method":"eth_createAccessList","params":['"$TRACE_TX"',"'"$HIST_HASH"'"]},
  {"jsonrpc":"2.0","id":2,"method":"debug_traceCall","params":['"$TRACE_TX"',"'"$HIST_HASH"'",{"tracer":"prestateTracer","enableReturnData":true}]}
]'
    R=$(rpc_batch "$TRACE_URL" "$BATCH_BODY")
    check_batch "Batch: createAccessList + traceCall (trace_and_access_list)" "$R" 2

fi # end dci-only / all

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "==========================================="
printf "Results: \033[32m%d passed\033[0m, \033[31m%d failed\033[0m, \033[33m%d warnings\033[0m\n" \
    "$PASS" "$FAIL" "$WARN"
echo "==========================================="

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "Required RPC capabilities are missing. Review failures above."
    exit 1
fi

if [ "$WARN" -gt 0 ]; then
    echo ""
    echo "All required capabilities present. Warnings are for methods the indexer does not depend on."
fi
