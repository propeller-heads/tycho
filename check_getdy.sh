#!/bin/bash
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
TRI=0xf5f5b97624542d72a9e06f04804bf81baa15e2b4
for blk in 25708548 25720000; do
  echo "block $blk:"
  echo "  get_dy(0,1,1e9)  = $(cast call --rpc-url "$rpc" --block $blk $TRI 'get_dy(uint256,uint256,uint256)(uint256)' 0 1 1000000000)"
  echo "  get_dy(0,2,1e9)  = $(cast call --rpc-url "$rpc" --block $blk $TRI 'get_dy(uint256,uint256,uint256)(uint256)' 0 2 1000000000)"
  echo "  get_dy(1,0,1e7)  = $(cast call --rpc-url "$rpc" --block $blk $TRI 'get_dy(uint256,uint256,uint256)(uint256)' 1 0 10000000)"
  echo "  get_dy(2,0,1e18) = $(cast call --rpc-url "$rpc" --block $blk $TRI 'get_dy(uint256,uint256,uint256)(uint256)' 2 0 1000000000000000000)"
done
