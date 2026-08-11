#!/bin/bash
# Probe MATH() addresses + versions for representative Curve CryptoSwap pools.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
probe() {
  local label="$1" pool="$2"
  local math version pver
  math=$(cast call --rpc-url "$rpc" "$pool" 'MATH()(address)' 2>/dev/null || echo "-")
  pver=$(cast call --rpc-url "$rpc" "$pool" 'version()(string)' 2>/dev/null || echo "-")
  if [ "$math" != "-" ]; then
    version=$(cast call --rpc-url "$rpc" "$math" 'version()(string)' 2>/dev/null || echo "-")
  else
    version="-"
  fi
  echo "$label pool=$pool poolversion=$pver math=$math mathversion=$version"
}
probe TricryptoUSDT 0xf5f5b97624542d72a9e06f04804bf81baa15e2b4
probe Tricrypto2V1 0xd51a44d3fae010294c616388b506acda1bfaae46
probe CRVETH_V1 0x8301ae4fc9c624d1d396cbdaa1ed877821d7c511
probe TwoNG_crvusd_fxn 0x390f3595bca2df7d23783dfd126427cceb997bf4
probe TwoNG_usdc_crvusd 0x4dece678ceceb27446b35c672dc7d61f30bad69e
