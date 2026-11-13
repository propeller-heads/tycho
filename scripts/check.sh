#!/usr/bin/env bash
set -euo pipefail
: "${RUST_NIGHTLY:?is set in mise.toml; activate mise or run this through mise exec --}"

cargo +"$RUST_NIGHTLY" fmt --all --check
cargo +"$RUST_NIGHTLY" clippy --workspace --all --all-features --all-targets -- -D warnings
cargo nextest run --workspace --all-targets --all-features -E 'not test(serial_db)'
cargo nextest run --workspace --all-targets --all-features -E 'test(serial_db)'

(cd crates/tycho-execution/contracts && forge fmt --check)
(cd crates/tycho-execution/contracts && forge test)
(cd protocols/adapter-integration/evm && forge fmt --check)
(cd protocols/adapter-integration/evm && forge test)
(cd crates/tycho-simulation/token-proxy-contracts && forge fmt --check)
(cd crates/tycho-simulation/token-proxy-contracts && forge test)
