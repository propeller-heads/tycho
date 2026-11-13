#!/usr/bin/env bash
set -e
: "${RUST_NIGHTLY:?is set in mise.toml; activate mise or run this through mise exec --}"

cargo +"$RUST_NIGHTLY" fmt -- --check
cargo +"$RUST_NIGHTLY" clippy --all --all-features --all-targets -- -D warnings
cargo build --target wasm32-unknown-unknown --all-targets --all-features
cargo test --workspace --all-targets --all-features
