#!/bin/bash
# Collect ground-truth newton_D values from deployed MATH contracts.
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
TRI=0xf5f5b97624542d72a9e06f04804bf81baa15e2b4
TRIMATH=0xcBFf3004a20dBfE2731543AA38599A526e0fD6eE
BLK=25708548

c() { cast call --rpc-url "$rpc" --block "$BLK" "$@"; }

echo "== TricryptoUSDT state at ramp-active block $BLK =="
b0=$(c $TRI 'balances(uint256)(uint256)' 0); b0=${b0%% *}
b1=$(c $TRI 'balances(uint256)(uint256)' 1); b1=${b1%% *}
b2=$(c $TRI 'balances(uint256)(uint256)' 2); b2=${b2%% *}
ps0=$(c $TRI 'price_scale(uint256)(uint256)' 0); ps0=${ps0%% *}
ps1=$(c $TRI 'price_scale(uint256)(uint256)' 1); ps1=${ps1%% *}
A=$(c $TRI 'A()(uint256)'); A=${A%% *}
g=$(c $TRI 'gamma()(uint256)'); g=${g%% *}
D=$(c $TRI 'D()(uint256)'); D=${D%% *}
t=$(c $TRI 'future_A_gamma_time()(uint256)'); t=${t%% *}
echo "balances=$b0 $b1 $b2"
echo "price_scale=$ps0 $ps1"
echo "A=$A gamma=$g storedD=$D future_A_gamma_time=$t"
# precisions: USDT 6 -> 1e12, WBTC 8 -> 1e10, WETH 18 -> 1
xp0=$(python3 -c "print($b0 * 10**12)")
xp1=$(python3 -c "print($b1 * $ps0 * 10**10 // 10**18)")
xp2=$(python3 -c "print($b2 * $ps1 * 1 // 10**18)")
echo "xp=$xp0 $xp1 $xp2"
newD=$(c $TRIMATH 'newton_D(uint256,uint256,uint256[3],uint256)(uint256)' "$A" "$g" "[$xp0,$xp1,$xp2]" 0)
echo "onchain newton_D=$newD"

echo "== TwoCrypto-NG v2.0.0 math: synthetic + real =="
TWOMATH=0x2005995a71243be9FB995DaB4742327dc76564Df
TWOMATH21=0x1Fd8Af16DC4BEBd950521308D55d0543b6cDF4A1
POOL2=0x004C167d27ADa24305b76D80762997Fa6EB8d9B2
p2b0=$(c $POOL2 'balances(uint256)(uint256)' 0); p2b0=${p2b0%% *}
p2b1=$(c $POOL2 'balances(uint256)(uint256)' 1); p2b1=${p2b1%% *}
p2ps=$(c $POOL2 'price_scale()(uint256)'); p2ps=${p2ps%% *}
p2A=$(c $POOL2 'A()(uint256)'); p2A=${p2A%% *}
p2g=$(c $POOL2 'gamma()(uint256)'); p2g=${p2g%% *}
p2prec=$(c $POOL2 'precisions()(uint256[2])')
echo "pool2 balances=$p2b0 $p2b1 price_scale=$p2ps A=$p2A gamma=$p2g precisions=$p2prec"
# assume 18/18 decimals unless precisions says otherwise (printed above)
xq0=$p2b0
xq1=$(python3 -c "print($p2b1 * $p2ps // 10**18)")
echo "xp2=$xq0 $xq1"
echo "onchain v200 newton_D=$(c $TWOMATH 'newton_D(uint256,uint256,uint256[2],uint256)(uint256)' "$p2A" "$p2g" "[$xq0,$xq1]" 0)"
echo "onchain v210 newton_D=$(c $TWOMATH21 'newton_D(uint256,uint256,uint256[2],uint256)(uint256)' "$p2A" "$p2g" "[$xq0,$xq1]" 0)"

echo "== synthetic balanced 2-coin =="
sx0=5000000000000000000000
sx1=5000000000000000000000
for m in $TWOMATH $TWOMATH21; do
  echo "math=$m D=$(c "$m" 'newton_D(uint256,uint256,uint256[2],uint256)(uint256)' 400000 145000000000000 "[$sx0,$sx1]" 0)"
done

echo "== StableswapMath v0.1.0 / v0.1.1 =="
S010=0x79839c2D74531A8222C0F555865aAc1834e82e51
S011=0xBfDdF58Cb6ef84e115fF47c10e49A80B2653EA13
for m in $S010 $S011; do
  echo "math=$m D=$(c "$m" 'newton_D(uint256,uint256,uint256[2],uint256)(uint256)' 350000 100000000000000 "[250289528581622891700521,249710772232236449374974]" 0)"
done

echo "== synthetic imbalanced tricrypto =="
echo "tri synthetic D=$(c $TRIMATH 'newton_D(uint256,uint256,uint256[3],uint256)(uint256)' 1707629 11809167828997 "[3000000000000000000000000,2400000000000000000000000,2600000000000000000000000]" 0)"
