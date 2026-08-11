#!/bin/bash
# Timestamps for candidate sweep blocks + ramp state probes.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
for b in 25660000 25680000 25700000 25708548 25720000 25750000 19010000 19020000 13378096 16408000 16435000 16450000; do
  ts=$(cast block --rpc-url "$rpc" "$b" -f timestamp 2>/dev/null || echo "-")
  echo "block $b ts=$ts"
done
echo "== ramp state probes =="
echo "tri crvUSD_T at 19010000: $(cast call --rpc-url "$rpc" --block 19010000 0x2889302a794da87fbf1d6db415c1492194663d13 'future_A_gamma_time()(uint256)' 2>/dev/null || echo unavailable)"
echo "cbETHETH at 16408000: $(cast call --rpc-url "$rpc" --block 16408000 0x5fae7e604fc3e24fd43a72867cebac94c65b404a 'future_A_gamma_time()(uint256)' 2>/dev/null || echo unavailable)"
echo "cbETHETH at 16435000: $(cast call --rpc-url "$rpc" --block 16435000 0x5fae7e604fc3e24fd43a72867cebac94c65b404a 'future_A_gamma_time()(uint256)' 2>/dev/null || echo unavailable)"
echo "cbETHETH at 16450000: $(cast call --rpc-url "$rpc" --block 16450000 0x5fae7e604fc3e24fd43a72867cebac94c65b404a 'future_A_gamma_time()(uint256)' 2>/dev/null || echo unavailable)"
echo "tricrypto2 at 13378096: $(cast call --rpc-url "$rpc" --block 13378096 0xd51a44d3fae010294c616388b506acda1bfaae46 'future_A_gamma_time()(uint256)' 2>/dev/null || echo unavailable)"
