#!/usr/bin/env bash
set -euo pipefail

# Regenerates the TychoRouterV3 and executor runtime bytecode fixtures used by
# protocol integration testing from the current tycho-execution contracts.
#
# Each fixture is produced by deploying the contract against a mainnet fork
# (via scripts/export-runtime-bytecode.js) and capturing its runtime bytecode,
# which bakes in the constructor immutables (permit2, pool managers, etc.).
#
# Modes:
#   (default)  Regenerate every fixture and write it to fixtures/.
#   --check    Regenerate into a temp dir and diff against the committed
#              fixtures. Exits non-zero if any fixture is out of date. Use this
#              in CI to detect when the contracts have changed.

usage() {
    cat <<EOF
Usage: $0 [--check]

Options:
  --check    Verify committed fixtures match the current contracts (no writes).
             Exits non-zero on drift.

Requires:
  RPC_URL    Ethereum mainnet RPC (the router constructor checks that permit2
             and the fee calculator have deployed code, so a fork is needed).
  forge, anvil, cast, node (with contracts/ npm deps installed).
EOF
    exit 1
}

CHECK_ONLY=0
case "${1:-}" in
"") ;;
--check) CHECK_ONLY=1 ;;
*) usage ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TESTING_DIR="$(dirname "$SCRIPT_DIR")"
REPO_ROOT="$(cd "$TESTING_DIR/../.." && pwd)"
CONTRACTS_DIR="$REPO_ROOT/crates/tycho-execution/contracts"
FIXTURES_DIR="$TESTING_DIR/fixtures"
EXPORT_SCRIPT="$CONTRACTS_DIR/scripts/export-runtime-bytecode.js"
DEPLOY_CONFIG="$CONTRACTS_DIR/../config/executor_deployments.json"

# Pin the fork to a fixed block for reproducibility. TychoRouterV3 (via EIP712)
# and UniswapV4Executor bake address(this) into an immutable at deploy time, and
# the export deploys via nonce-based CREATE — so the deployer's mainnet nonce
# (which moves over time) would otherwise change the deploy address, and thus
# the bytecode, on every run. Pinning the block fixes the nonce and makes
# generation deterministic, which --check relies on.
FORK_BLOCK=21000000

PERMIT2="0x000000000022D473030F116dDEE9F6B43aC78BA3"
ADMIN="0x0000000000000000000000000000000000000001"

# fixture | contract | literal constructor args
#
# TychoRouterV3(permit2, feeCalculator, pauserAdmin, unpauserAdmin,
#             executorSetterAdmin, routerFeeSetterAdmin):
#   - permit2 is the only immutable, so it is the only arg baked into the
#     bytecode; it must be the canonical Permit2.
#   - feeCalculator only needs deployed code (constructor reverts otherwise), so
#     Permit2 is reused; the real one is set via storage at simulation time.
#   - the four admins are role grants (storage), so any placeholder works.
NON_EXECUTOR_FIXTURES=(
    "TychoRouterV3|TychoRouterV3|$PERMIT2 $PERMIT2 $ADMIN $ADMIN $ADMIN $ADMIN"
    "FeeCalculator|FeeCalculator|$ADMIN"
)

# Executor fixtures protocol-testing plants, mirroring EXECUTOR_MAPPING in
# protocols/testing/src/execution.rs. The contract name and constructor args
# are resolved from executor_deployments.json by (chain, protocol).
# fixture | chain | protocol
EXECUTOR_FIXTURES=(
    "UniswapV2|ethereum|uniswap_v2"
    "UniswapV3|ethereum|uniswap_v3"
    "UniswapV4|ethereum|uniswap_v4"
    "UniswapV4Angstrom|ethereum|uniswap_v4"
    "BalancerV2|ethereum|vm:balancer_v2"
    "BalancerV3|ethereum|vm:balancer_v3"
    "Curve|ethereum|vm:curve"
    "FermiSwap|ethereum|vm:fermiswap"
    "MaverickV2|ethereum|vm:maverick_v2"
    "EkuboV3|ethereum|ekubo_v3"
    "FluidV1|ethereum|fluid_v1"
    "LiquidityParty|ethereum|vm:liquidityparty"
    "LunarBase|base|lunarbase"
    "RingSwapV2|ethereum|ring_swap_v2"
    "RingSwapV2Bsc|bsc|ring_swap_v2"
)

if [[ -z "${RPC_URL:-}" ]]; then
    echo "Error: RPC_URL must be set (Ethereum mainnet RPC)." >&2
    exit 1
fi

for bin in forge anvil cast node; do
    command -v "$bin" >/dev/null 2>&1 || {
        echo "Error: '$bin' not found in PATH." >&2
        exit 1
    }
done

WORK_DIR="$(mktemp -d)"
ANVIL_PID=""
cleanup() {
    [[ -n "$ANVIL_PID" ]] && kill "$ANVIL_PID" 2>/dev/null || true
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT

echo "Building contracts..."
(cd "$CONTRACTS_DIR" && forge build >/dev/null)

echo "Starting anvil fork (block $FORK_BLOCK)..."
anvil --fork-url "$RPC_URL" --fork-block-number "$FORK_BLOCK" --silent &
ANVIL_PID=$!
# Probe the local anvil explicitly: cast otherwise falls back to $ETH_RPC_URL
# when set (e.g. in CI), which would query mainnet instead of the local fork.
LOCAL_RPC="http://127.0.0.1:8545"
for _ in $(seq 1 30); do
    if cast block-number --rpc-url "$LOCAL_RPC" >/dev/null 2>&1; then break; fi
    sleep 1
done
cast block-number --rpc-url "$LOCAL_RPC" >/dev/null 2>&1 || {
    echo "Error: anvil did not become ready." >&2
    exit 1
}

# Reads the runtime bytecode from an export-runtime-bytecode.js output file and
# writes it as minified single-line JSON.
minify() {
    python3 - "$1" "$2" <<'PY'
import json, sys
bytecode = json.load(open(sys.argv[1]))["runtimeBytecode"]
with open(sys.argv[2], "w") as f:
    f.write(json.dumps({"runtimeBytecode": bytecode}, separators=(",", ":")))
PY
}

# Prints "<contract> <arg1> <arg2> ..." for a (chain, protocol) pair, read from
# executor_deployments.json.
resolve_deployment() {
    node -e "
const cfg = require('$DEPLOY_CONFIG');
const e = (cfg[process.argv[1]] || {})[process.argv[2]];
if (!e) {
    console.error('Missing ' + process.argv[1] + '/' + process.argv[2] + ' in executor_deployments.json');
    process.exit(1);
}
process.stdout.write([e.contract].concat((e.args || []).map(String)).join(' '));
" "$1" "$2"
}

# Generates one fixture: deploys <contract> with the given constructor args,
# captures its runtime bytecode, and either writes it to fixtures/ or (in
# --check mode) diffs it against the committed fixture.
process_fixture() {
    fixture="$1"
    contract="$2"
    shift 2

    echo "Generating $fixture (from $contract)..."
    node "$EXPORT_SCRIPT" "$contract" "$@" >/dev/null

    export_out="$CONTRACTS_DIR/test/$contract.runtime.json"
    generated="$WORK_DIR/$fixture.runtime.json"
    minify "$export_out" "$generated"
    rm -f "$export_out"

    target="$FIXTURES_DIR/$fixture.runtime.json"
    if [[ "$CHECK_ONLY" -eq 1 ]]; then
        if ! diff -q "$target" "$generated" >/dev/null 2>&1; then
            echo "  DRIFT: $fixture.runtime.json is out of date"
            DRIFT=1
        fi
    else
        cp "$generated" "$target"
    fi
}

DRIFT=0
for entry in "${NON_EXECUTOR_FIXTURES[@]}"; do
    IFS='|' read -r fixture contract litargs <<<"$entry"
    # shellcheck disable=SC2086 # litargs is an intentionally word-split arg list
    process_fixture "$fixture" "$contract" $litargs
done
for entry in "${EXECUTOR_FIXTURES[@]}"; do
    IFS='|' read -r fixture chain protocol <<<"$entry"
    deployment="$(resolve_deployment "$chain" "$protocol")" || exit 1
    read -r contract args <<<"$deployment"
    # shellcheck disable=SC2086 # args is an intentionally word-split arg list
    process_fixture "$fixture" "$contract" $args
done

if [[ "$CHECK_ONLY" -eq 1 ]]; then
    if [[ "$DRIFT" -eq 1 ]]; then
        echo "Fixtures are out of date. Run: $0" >&2
        exit 1
    fi
    echo "All fixtures are up to date."
else
    echo "Done. Fixtures written to $FIXTURES_DIR"
fi
