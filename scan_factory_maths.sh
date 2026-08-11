#!/bin/bash
# Collect distinct MATH addresses across twocrypto-ng factory pools (sampled).
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
factory=0x98EE851a00abeE0d95D08cF4CA2BdCE32aeaAF7F
count=$(cast call --rpc-url "$rpc" "$factory" 'pool_count()(uint256)')
count=${count%% *}
seenfile=$(mktemp)
for i in $(seq 0 10 $((count - 1))); do
  pool=$(cast call --rpc-url "$rpc" "$factory" "pool_list(uint256)(address)" "$i" 2>/dev/null) || continue
  math=$(cast call --rpc-url "$rpc" "$pool" 'MATH()(address)' 2>/dev/null) || continue
  if ! grep -q "$math" "$seenfile"; then
    echo "$math" >> "$seenfile"
    mver=$(cast call --rpc-url "$rpc" "$math" 'version()(string)' 2>/dev/null || echo "-")
    echo "distinct math=$math version=$mver first_seen_pool=$pool idx=$i"
  fi
done
trash "$seenfile" 2>/dev/null || true
