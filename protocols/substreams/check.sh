#!/usr/bin/env bash
set -e

cargo +nightly fmt -- --check
cargo +nightly clippy --all --all-features --all-targets -- -D warnings
cargo build --target wasm32-unknown-unknown --all-targets --all-features
cargo test --workspace --all-targets --all-features
