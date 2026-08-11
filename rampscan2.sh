#!/bin/bash
# Scan CryptoSwap pools for future_A_gamma_time values (ramp history).
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
now=$(date +%s)
echo "now=$now"
pools="
tri_ng_USDT:0xf5f5b97624542d72a9e06f04804bf81baa15e2b4
tri_ng_USDC:0x7f86bf177dd4f3494b841a37e810a34dd56c829b
tri_ng_crvUSD_T:0x2889302a794da87fbf1d6db415c1492194663d13
tri_ng_TriCRV:0x4ebdf703948ddcea3b11f675b4d1fba9d2414a14
tri_v1_tricrypto2:0xd51a44d3fae010294c616388b506acda1bfaae46
two_v1_CVXETH:0xb576491f1e6e5e62f1d8f26062ee822b40b0e0d4
two_v1_TETH:0x752ebeb79963cf0732e9c0fec72a49fd1defaeac
two_v1_YFIETH:0xc26b89a667578ec7b3f11b2f98d6fd15c07c54ba
two_v1_cbETHETH:0x5fae7e604fc3e24fd43a72867cebac94c65b404a
two_ng_0:0x004C167d27ADa24305b76D80762997Fa6EB8d9B2
two_ng_1:0x6A1C781B7B280E3c8BF04FDfb86C112C9Ac70a89
two_ng_2:0x5f0985A8aAd85e82fD592a23Cc0501e4345fb18c
two_ng_3:0x0fF26A978A61d40F6591fc700EF878E96aF6C2C0
two_ng_4:0xca546aE6c3B2BB9Fba2b6e5EeB0881097CecE5B0
two_ng_v210:0x8E001d4BAC0EaE1eea348dFC22f9B8bDA67dd211
two_stable_010:0xFc997dd1c746C5333139eFEF80db2A55004f98DC
two_stable_011:0x4FdcCB810f22578ad6700fC10a8C9B6c1DF61852
"
for entry in $pools; do
  label="${entry%%:*}"
  addr="${entry##*:}"
  t=$(cast call --rpc-url "$rpc" "$addr" 'future_A_gamma_time()(uint256)' 2>/dev/null || echo "-")
  t=${t%% *}
  echo "$label $addr future_A_gamma_time=$t"
done
