#!/bin/bash
# Run the copied repro crate against the fixed worktree, scrubbing the RPC URL.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
cd "$dir/../repro-tycho-curve-fixcheck"
RPC_URL="$rpc" cargo test --test curve_ramp_mispricing -- --nocapture 2>&1 \
  | sed "s#${rpc}#<RPC>#g" | tail -40
