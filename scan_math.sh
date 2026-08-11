#!/bin/bash
# Scan the sibling investigation's pool list: which have MATH() and what version.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
while read -r pool; do
  [ -z "$pool" ] && continue
  math=$(cast call --rpc-url "$rpc" "$pool" 'MATH()(address)' 2>/dev/null || echo "-")
  if [ "$math" = "-" ]; then
    gamma=$(cast call --rpc-url "$rpc" "$pool" 'gamma()(uint256)' 2>/dev/null || echo "-")
    echo "$pool math=- gamma=$gamma"
    continue
  fi
  mver=$(cast call --rpc-url "$rpc" "$math" 'version()(string)' 2>/dev/null || echo "-")
  ncoins=$(cast call --rpc-url "$rpc" "$pool" 'N_COINS()(uint256)' 2>/dev/null || echo "-")
  echo "$pool math=$math mathversion=$mver ncoins=$ncoins"
done < "$dir/../curve_ids.txt"
