#!/usr/bin/env bash
# Builds a Substreams package with an embedded seed and packs it as an spkg whose initialBlock is
# the seed block.
#
# Usage: pack-seeded.sh <protocol-dir> <seed.bin> [output.spkg]
#
# <protocol-dir> holds the package in `package/` and its seed writer, a native crate, in `seed/`.
# The writer provides the seed block and the manifest rewrite; it is run through cargo from the
# repository root, so only `substreams` has to be on PATH. The seed is copied over the package's
# committed `seed.bin` (an empty seed pinned to a block the stock manifest never streams) for the
# build, and that file is restored afterwards, so a real seed never ends up in git.
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: $0 <protocol-dir> <seed.bin> [output.spkg]" >&2
    exit 1
fi

protocol_dir=$(cd "$1" && pwd)
package_dir="$protocol_dir/package"
seed=$(realpath "$2")
protocol=$(basename "$protocol_dir")
repo_root=$(cd "$(dirname "$0")/../.." && pwd)

seed_cli() {
    (cd "$repo_root" && cargo run --quiet --release --manifest-path "$protocol_dir/seed/Cargo.toml" -- "$@")
}

block=$(seed_cli inspect "$seed" --block-number)
version=$(awk -F'"' '/^version = / { print $2; exit }' "$package_dir/Cargo.toml")
output=${3:-"$repo_root/docker/substreams/$protocol-v$version-seed$block.spkg"}

cd "$package_dir"
cp "$seed" seed.bin
trap 'git checkout -- seed.bin' EXIT

cargo build --release --target wasm32-unknown-unknown
seed_cli manifest --seed "$seed" --in "$package_dir/substreams.yaml" --out "$package_dir/substreams.seeded.yaml"
substreams pack substreams.seeded.yaml -o "$output"

echo "packed $output (initialBlock $block)"
