set -e
: "${RUST_NIGHTLY:?is set in mise.toml; activate mise or run this through mise exec --}"

cargo +"$RUST_NIGHTLY" fmt -- --check
cargo +"$RUST_NIGHTLY" clippy --locked --all --all-features --all-targets -- -D warnings
cargo nextest run --workspace --locked --all-targets --all-features --bin tycho-indexer -E 'not test(serial_db)'
cargo nextest run --workspace --locked --all-targets --all-features --bin tycho-indexer -E 'test(serial_db)'
