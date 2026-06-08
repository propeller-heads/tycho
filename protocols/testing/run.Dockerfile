# =========== Third Party ===========
# Get substreams CLI
FROM ghcr.io/streamingfast/substreams:v1.16.4 AS substreams-cli

# Install Foundry (Forge)
FROM debian:bookworm AS foundry-builder
WORKDIR /build
RUN apt-get update && apt-get install -y curl git
RUN curl -L https://foundry.paradigm.xyz | bash
RUN /root/.foundry/bin/foundryup

# =========== Protocol SDK (includes tycho-indexer) ===========
FROM rust:1.89-bookworm AS protocol-sdk-builder
ARG PROTOCOLS=""
WORKDIR /build/tycho-protocol-sdk
COPY . .

# Build EVM contracts first (protocol-testing depends on the runtime JSON files)
WORKDIR /build/tycho-protocol-sdk/protocols/adapter-integration/evm
COPY --from=foundry-builder /root/.foundry/bin/forge /usr/local/bin/forge
RUN chmod +x /usr/local/bin/forge
# Fetch forge lib submodules (not present in Docker context due to .dockerignore
# excluding .git/ and CI checkout not always fetching submodules).
RUN apt-get update && apt-get install -y --no-install-recommends git && \
    git init && \
    rm -rf lib/forge-std lib/openzeppelin-contracts && \
    forge install foundry-rs/forge-std OpenZeppelin/openzeppelin-contracts --no-git && \
    apt-get purge -y git && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*
RUN forge build

# Build substreams (wasm targets only - source not needed in final image)
WORKDIR /build/tycho-protocol-sdk/protocols/substreams
# resolve_base maps clone protocol names to their base substreams directory.
# Keep this in sync with CLONE_TO_BASE_PROTOCOL in protocols/testing/src/test_runner.rs.
RUN resolve_base() { \
        case "$1" in \
            base-alienbase-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            base-baseline) echo "ethereum-baseline" ;; \
            ethereum-pancakeswap-v2) echo "ethereum-uniswap-v2" ;; \
            ethereum-sushiswap-v2) echo "ethereum-uniswap-v2" ;; \
            unichain-curve) echo "ethereum-curve" ;; \
            *) echo "$1" ;; \
        esac; \
    }; \
    if [ -n "$PROTOCOLS" ]; then \
        echo "Building only specified protocols: $PROTOCOLS"; \
        for protocol in $PROTOCOLS; do \
            protocol_clean=${protocol%%=*}; \
            base_dir=$(resolve_base "$protocol_clean"); \
            if [ -d "$base_dir" ]; then \
                echo "Building $base_dir (for protocol $protocol_clean)..."; \
                cd "$base_dir" && cargo build --target wasm32-unknown-unknown --release && cd ..; \
            fi; \
        done; \
    else \
        echo "Building all protocols..."; \
        cargo build --target wasm32-unknown-unknown --release; \
    fi

# Build tycho-indexer binary (now part of the monorepo)
WORKDIR /build/tycho-protocol-sdk
RUN cargo build --release --bin tycho-indexer

# Build protocol-testing binary (after EVM contracts are built)
WORKDIR /build/tycho-protocol-sdk/protocols/testing
RUN cargo build --release

# =========== Substreams Filter Stage ===========
FROM debian:bookworm-slim AS substreams-filter
ARG PROTOCOLS=""
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/substreams /source
# resolve_base maps clone protocol names to their base substreams directory.
# Keep this in sync with CLONE_TO_BASE_PROTOCOL in protocols/testing/src/test_runner.rs.
RUN resolve_base() { \
        case "$1" in \
            base-alienbase-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            base-baseline) echo "ethereum-baseline" ;; \
            ethereum-pancakeswap-v2) echo "ethereum-uniswap-v2" ;; \
            ethereum-sushiswap-v2) echo "ethereum-uniswap-v2" ;; \
            unichain-curve) echo "ethereum-curve" ;; \
            *) echo "$1" ;; \
        esac; \
    }; \
    mkdir -p /filtered/target/wasm32-unknown-unknown/release && \
    if [ -n "$PROTOCOLS" ]; then \
        echo "Filtering for protocols: $PROTOCOLS"; \
        for protocol in $PROTOCOLS; do \
            protocol_clean=${protocol%%=*}; \
            base_dir=$(resolve_base "$protocol_clean"); \
            if [ -d "/source/$base_dir" ]; then \
                echo "Including $base_dir (for protocol $protocol_clean)..."; \
                cp -r "/source/$base_dir" "/filtered/"; \
                base_wasm=$(echo "$base_dir" | tr '-' '_'); \
                if [ -f "/source/target/wasm32-unknown-unknown/release/${base_wasm}.wasm" ]; then \
                    cp "/source/target/wasm32-unknown-unknown/release/${base_wasm}.wasm" \
                       "/filtered/target/wasm32-unknown-unknown/release/"; \
                fi; \
            fi; \
        done; \
    else \
        echo "Including all protocols..."; \
        cp -r /source/* /filtered/; \
    fi && \
    echo "Filter stage complete. Size:" && du -sh /filtered 2>/dev/null || echo "Filter stage complete"

# =========== Final Runtime Image ===========
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates libssl3 libpq5 && \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/* /usr/share/doc/* /usr/share/man/* /usr/share/locale/* && \
    find /usr/lib -name "*.a" -delete && \
    find /usr/lib -name "*.la" -delete

# Copy essential binaries only
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/target/release/tycho-indexer /usr/local/bin/tycho-indexer
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/target/release/protocol-testing /usr/local/bin/tycho-protocol-sdk
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/testing/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Create minimal directory structure matching expected layout:
# The test runner looks for <root>/substreams/ and <root>/adapter-integration/evm/
RUN mkdir -p /proto /app/adapter-integration/evm

# Copy proto files (needed for substreams pack)
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/proto /proto

# Copy EVM directory
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/adapter-integration/evm/out /app/adapter-integration/evm/out
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/adapter-integration/evm/scripts /app/adapter-integration/evm/scripts
# Remove unnecessary EVM build artifacts
RUN find /app/adapter-integration/evm/out -name "*.json" ! -name "*.runtime.json" -delete && \
    find /app/adapter-integration/evm/out -type d -empty -delete 2>/dev/null || true

# Copy filtered substreams from filter stage
COPY --from=substreams-filter /filtered /app/substreams

# Clean up unnecessary files to reduce size
RUN find /app -name "*.rs" -delete && \
    find /app -name "Cargo.toml" -delete && \
    find /app -name "Cargo.lock" -delete && \
    find /app -name "src" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app -name "*.d" -delete && \
    find /app -name "*.rlib" -delete && \
    find /app -name "*.rmeta" -delete && \
    find /app -name ".fingerprint" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app -name "build" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app -name "deps" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app -name "incremental" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app -type d -empty -delete 2>/dev/null || true

# Copy external tools
COPY --from=substreams-cli /app/substreams /usr/local/bin/substreams
RUN chmod +x /usr/local/bin/substreams
COPY --from=foundry-builder /root/.foundry/bin/forge /usr/local/bin/forge
COPY --from=foundry-builder /root/.foundry/bin/cast /usr/local/bin/cast

# Strip binaries to reduce size
RUN strip /usr/local/bin/* 2>/dev/null || true

WORKDIR /app
ENTRYPOINT ["/entrypoint.sh"]
