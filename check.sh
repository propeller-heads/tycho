#!/usr/bin/env bash
set -e
: "${RUST_NIGHTLY:?is set in mise.toml; activate mise or run this through mise exec --}"

cargo +"$RUST_NIGHTLY" fmt --all --check
cargo +"$RUST_NIGHTLY" clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features -E 'not test(serial_db)'
cargo nextest run --workspace --all-features -E 'test(serial_db)'
