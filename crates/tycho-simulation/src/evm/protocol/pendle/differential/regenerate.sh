#!/usr/bin/env bash
# Regenerates the differential fixtures under ../tests/fixtures/.
#
# The committed fixtures are what the Rust tests assert against, so this only needs running when
# the grid changes or when Pendle's math does. It clones the upstream sources into lib/, which is
# gitignored — see ../NOTICE.md for why they are not vendored.
set -euo pipefail
cd "$(dirname "$0")"

clone() {
  local repo=$1 dir=$2 tag=${3:-}
  if [ -d "$dir" ]; then
    return
  fi
  if [ -n "$tag" ]; then
    echo "cloning $repo @ $tag"
    git clone --depth 1 --branch "$tag" --quiet "$repo" "$dir"
  else
    echo "cloning $repo"
    git clone --depth 1 --quiet "$repo" "$dir"
  fi
}

clone https://github.com/pendle-finance/pendle-core-v2-public.git lib/pendle-core-v2-public
clone https://github.com/foundry-rs/forge-std.git lib/forge-std
# Pendle's interfaces import OpenZeppelin. Pinned, because an unpinned major would change the
# interfaces underneath the fixtures.
clone https://github.com/OpenZeppelin/openzeppelin-contracts.git lib/openzeppelin-contracts v4.9.6

mkdir -p ../tests/fixtures
forge test --match-path "test/*Fixtures.t.sol" -vv

echo
echo "fixtures written:"
ls -la ../tests/fixtures/
