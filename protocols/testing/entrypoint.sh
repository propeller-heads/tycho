#!/bin/bash
set -e

# Check arguments
if [ "$#" -lt 1 ]; then
	echo "Usage: $0 protocol1[=filter] [protocol2 ...] or \"$0 'protocol1[=filter] protocol2'\""
	exit 1
fi
if [ "$#" -eq 1 ] && [[ "$1" == *" "* ]]; then
	IFS=' ' read -r -a args <<< "$1"
else
	args=("$@")
fi

# Check required binaries
errors=()
for bin in tycho-indexer tycho-protocol-sdk substreams forge cast; do
    if command -v "$bin" >/dev/null 2>&1; then
        "$bin" --version || echo "$bin does not support --version"
    else
        errors+=("Binary '$bin' not found in PATH")
    fi
done
if [ "${#errors[@]}" -ne 0 ]; then
    for err in "${errors[@]}"; do
        echo "$err"
    done
    exit 1
fi

# Infer the chain from the protocol name prefix.
infer_chain() {
    local protocol="$1"
    case "$protocol" in
        base-*)     echo "base" ;;
        arbitrum-*) echo "arbitrum" ;;
        unichain-*) echo "unichain" ;;
        bsc-*)      echo "bsc" ;;
        polygon-*)  echo "polygon" ;;
        *)          echo "ethereum" ;;
    esac
}

# Return the appropriate RPC URL for the given protocol.
# Chain-specific URLs fall back to the generic RPC_URL if not set.
get_rpc_url() {
    local protocol="$1"
    case "$protocol" in
        base-*)     echo "${BASE_RPC_URL:-$RPC_URL}" ;;
        arbitrum-*) echo "${ARBITRUM_RPC_URL:-$RPC_URL}" ;;
        unichain-*) echo "${UNICHAIN_RPC_URL:-$RPC_URL}" ;;
        bsc-*)      echo "${BSC_RPC_URL:-$RPC_URL}" ;;
        polygon-*)  echo "${POLYGON_RPC_URL:-$RPC_URL}" ;;
        *)          echo "$RPC_URL" ;;
    esac
}

# Test mode: "range" (block ranges from the yaml, default) or "full" (continuous sync from the
# initial block to the chain tip). Full mode runs indefinitely.
MODE="${MODE:-range}"
case "$MODE" in
	range|full) ;;
	*) echo "Invalid MODE '$MODE' (expected 'range' or 'full')"; exit 1 ;;
esac

# Run tests
for test in "${args[@]}"; do
	protocol="${test%%=*}"
	suffix="${test#*=}"
	chain=$(infer_chain "$protocol")
	rpc_url=$(get_rpc_url "$protocol")
	export RPC_URL="$rpc_url"
	echo "Running '$MODE' tests for protocol: $protocol (chain: $chain)"
	# --prebuilt-wasm: the builder stage compiled every WASM binary and this image ships no Rust
	# toolchain, so the packages must be packed as they are.
	cmd=(tycho-protocol-sdk "$MODE" --package "$protocol" --chain "$chain" \
		--rpc-url "$rpc_url" --db-url "$DATABASE_URL" --prebuilt-wasm)
	# The "=" suffix in PROTOCOLS means different things per mode:
	#   range → --match-test (run one named test case)
	#   full  → --initial-block (start syncing from this block)
	if [[ "$test" == *"="* ]]; then
		if [ "$MODE" = "full" ]; then
			cmd+=(--initial-block "$suffix")
		else
			cmd+=(--match-test "$suffix")
		fi
	fi
	"${cmd[@]}"
done
