#!/bin/bash
# Find historical ramp windows: initial times + RampAgamma logs on legacy pools.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"

echo "== initial_A_gamma_time =="
for p in 0xf5f5b97624542d72a9e06f04804bf81baa15e2b4 0x2889302a794da87fbf1d6db415c1492194663d13; do
  it=$(cast call --rpc-url "$rpc" "$p" 'initial_A_gamma_time()(uint256)' 2>/dev/null || echo "-")
  echo "$p initial_A_gamma_time=$it"
done

sig='RampAgamma(uint256,uint256,uint256,uint256,uint256,uint256)'
echo "== RampAgamma logs (legacy 2-coin + tricrypto2) =="
for p in 0xb576491f1e6e5e62f1d8f26062ee822b40b0e0d4 0x752ebeb79963cf0732e9c0fec72a49fd1defaeac 0xc26b89a667578ec7b3f11b2f98d6fd15c07c54ba 0x5fae7e604fc3e24fd43a72867cebac94c65b404a 0xd51a44d3fae010294c616388b506acda1bfaae46; do
  echo "pool $p:"
  cast logs --rpc-url "$rpc" --from-block 13000000 --to-block latest --address "$p" "$sig" 2>/dev/null \
    | rg "blockNumber|data" | head -8
done
