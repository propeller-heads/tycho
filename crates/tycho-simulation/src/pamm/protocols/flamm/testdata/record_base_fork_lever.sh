#!/usr/bin/env bash
# Re-records base_fork_lever.json beside this script from a local anvil fork of Base at block
# 53,755,892 where an Efficient pool's leverage route wins small sales: the factory, pools, hook
# class and row that `fork.rs` names, both pools' hooks of that class. The ignored live test checks
# the fork as it records it, the sale's far edge and its control included, and fails on the first
# size it quotes past the pool's swap.
#
# Usage: RPC_URL=http://127.0.0.1:<port> ./record_base_fork_lever.sh
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
: "${RPC_URL:?RPC_URL names the local fork}"
export RPC_URL
FLAMM_FORK_EDGE=sale FLAMM_FORK_RECORD="$here/base_fork_lever.json" \
    cargo test -p tycho-simulation --lib --all-features -- --ignored --exact --nocapture \
    pamm::protocols::flamm::fork::a_lever_fork_quotes_no_size_past_its_swap
