#!/usr/bin/env bash
# Regenerates the differential fixtures under ../tests/fixtures/.
#
# The committed fixtures are what the Rust tests assert against, so this only needs running when
# the grid changes or when Pendle's math does. It clones the upstream sources into lib/, which is
# gitignored — see ../NOTICE.md for why they are not vendored.
set -euo pipefail
cd "$(dirname "$0")"

# Every source is pinned to the commit the committed fixtures were generated from, so a regeneration
# after an upstream change cannot move the Solidity the tests assert bit-equality against.
clone() {
  local repo=$1 dir=$2 commit=$3
  if [ -d "$dir" ]; then
    if [ "$(git -C "$dir" rev-parse HEAD)" != "$commit" ]; then
      echo "$dir is not at $commit; delete it and rerun" >&2
      exit 1
    fi
    return
  fi
  echo "cloning $repo @ $commit"
  git init --quiet "$dir"
  git -C "$dir" fetch --depth 1 --quiet "$repo" "$commit"
  git -C "$dir" checkout --quiet FETCH_HEAD
}

clone https://github.com/pendle-finance/pendle-core-v2-public.git lib/pendle-core-v2-public \
  0fcebf79fa7d9eced3137c8d09829e315ed50b3c
clone https://github.com/foundry-rs/forge-std.git lib/forge-std \
  680ee6692649dcc7c617e05b2144932618264a83
# Pendle's interfaces import OpenZeppelin, at v4.9.6.
clone https://github.com/OpenZeppelin/openzeppelin-contracts.git lib/openzeppelin-contracts \
  dc44c9f1a4c3b10af99492eed84f83ed244203f6

mkdir -p ../tests/fixtures
forge test --match-path "test/*Fixtures.t.sol" -vv

echo
echo "fixtures written:"
ls -la ../tests/fixtures/
