#!/usr/bin/env bash
set -euo pipefail

# Builds the Curve substreams package from origin/main.
#
# The parity integration test uses the substreams output as ground truth. Building
# the spkg from main (in a temporary git worktree) instead of the working tree
# guarantees the test compares the native processor against the substreams state
# deployed from main, even when the current branch modifies the substreams source.
#
# Usage: build_main_spkg.sh [output-path]
#   output-path  defaults to <repo>/target/spkg/ethereum-curve-main.spkg

repo_root=$(git rev-parse --show-toplevel)
out="${1:-$repo_root/target/spkg/ethereum-curve-main.spkg}"

worktree=$(mktemp -d)/main
git -C "$repo_root" fetch origin main
git -C "$repo_root" worktree add --detach "$worktree" origin/main >/dev/null
trap 'git -C "$repo_root" worktree remove --force "$worktree"' EXIT

cd "$worktree/protocols/substreams/ethereum-curve"
cargo build --target wasm32-unknown-unknown --release -p ethereum-curve

mkdir -p "$(dirname "$out")"
substreams pack ethereum-curve.yaml -o "$out"
echo "spkg written to $out"
