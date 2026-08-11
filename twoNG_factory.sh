#!/bin/bash
# Enumerate twocrypto-ng factory pools and their MATH versions (first 25).
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
factory=0x98EE851a00abeE0d95D08cF4CA2BdCE32aeaAF7F
count=$(cast call --rpc-url "$rpc" "$factory" 'pool_count()(uint256)')
echo "pool_count=$count"
n=25
for i in $(seq 0 $((n - 1))); do
  pool=$(cast call --rpc-url "$rpc" "$factory" "pool_list(uint256)(address)" "$i" 2>/dev/null || echo "-")
  [ "$pool" = "-" ] && continue
  math=$(cast call --rpc-url "$rpc" "$pool" 'MATH()(address)' 2>/dev/null || echo "-")
  mver=$(cast call --rpc-url "$rpc" "$math" 'version()(string)' 2>/dev/null || echo "-")
  ramp=$(cast call --rpc-url "$rpc" "$pool" 'future_A_gamma_time()(uint256)' 2>/dev/null || echo "-")
  echo "$i $pool math=$math mathversion=$mver future_A_gamma_time=$ramp"
done
