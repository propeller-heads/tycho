# =========== Third Party ===========
# substreams CLI. Integration tests run v1.16.4; mise.toml pins the version the release
# workflows use.
FROM ghcr.io/streamingfast/substreams:v1.16.4 AS substreams-cli

# Tools pinned in mise.toml, installed from mise.lock: mise refuses any version not locked there.
FROM jdxcode/mise:2026.10.25-debian@sha256:00a8a9d05a300e5a15a89d6d3a87ea88ec4e78c4702b78d49aa1d3250af61427 AS tools
WORKDIR /tools
COPY mise.toml mise.lock ./
ENV MISE_TRUSTED_CONFIG_PATHS=/tools
RUN mise install foundry \
    && mkdir /out \
    && cp "$(mise which forge)" "$(mise which cast)" /out/

# =========== Protocol SDK (includes tycho-indexer) ===========
FROM rust:1.99.1-bookworm AS protocol-sdk-builder
ARG PROTOCOLS=""
WORKDIR /build/tycho-protocol-sdk
COPY . .

# Build substreams (wasm targets only - source not needed in final image)
WORKDIR /build/tycho-protocol-sdk/protocols/substreams
# resolve_base maps clone protocol names to buildable substreams directories.
# This follows CLONE_TO_BASE_PROTOCOL in test_runner.rs except that V4 clone aliases resolve to
# the parent workspace. The runtime uses ethereum-uniswap-v4/no-hooks, while build/filter must
# copy ethereum-uniswap-v4 so no-hooks and its parent-local target directory are both present.
RUN resolve_base() { \
        case "$1" in \
            bsc-lunarbase) echo "base-lunarbase" ;; \
            bsc-pancakeswap-infinity-bin) echo "base-pancakeswap-infinity-bin" ;; \
            base-alienbase-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            robinhood-sushiswap-v3|robinhood-robinswap-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            robinhood-ramses-v3|arbitrum-ramses-v3) echo "polygon-ramses-v3" ;; \
            robinhood-gigadex-v3|robinhood-swaphood-v3) echo "ethereum-pancakeswap-v3" ;; \
            elysium-kleos-v3|elysium-elyspump-v3|elysium-stockiumswap-v3|ethereum-communityv3) echo "ethereum-uniswap-v3-logs-only" ;; \
            robinhood-ekubo-v3|bsc-ekubo-v3|base-ekubo-v3) echo "ethereum-ekubo-v3" ;; \
            robinhood-up-v3) echo "base-aerodrome-slipstreams" ;; \
            elysium-aero-cl) echo "evm-aero-cl" ;; \
            base-balancer-v3|arbitrum-balancer-v3|gnosis-balancer-v3) echo "ethereum-balancer-v3" ;; \
            arc-uniswap-v2) echo "ethereum-uniswap-v2" ;; \
            arc-uniswap-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            arc-uniswap-v4-no-hooks) echo "ethereum-uniswap-v4" ;; \
            bsc-ring-swap-v2) echo "ethereum-ring-swap-v2" ;; \
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

# Build protocol-testing binary
WORKDIR /build/tycho-protocol-sdk/protocols/testing
RUN cargo build --release

# =========== Substreams Filter Stage ===========
FROM debian:bookworm-slim AS substreams-filter
ARG PROTOCOLS=""
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/substreams /source
# resolve_base maps clone protocol names to buildable substreams directories.
# This follows CLONE_TO_BASE_PROTOCOL in test_runner.rs except that V4 clone aliases resolve to
# the parent workspace. The runtime uses ethereum-uniswap-v4/no-hooks, while build/filter must
# copy ethereum-uniswap-v4 so no-hooks and its parent-local target directory are both present.
RUN resolve_base() { \
        case "$1" in \
            bsc-lunarbase) echo "base-lunarbase" ;; \
            bsc-pancakeswap-infinity-bin) echo "base-pancakeswap-infinity-bin" ;; \
            base-alienbase-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            robinhood-sushiswap-v3|robinhood-robinswap-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            robinhood-ramses-v3|arbitrum-ramses-v3) echo "polygon-ramses-v3" ;; \
            robinhood-gigadex-v3|robinhood-swaphood-v3) echo "ethereum-pancakeswap-v3" ;; \
            elysium-kleos-v3|elysium-elyspump-v3|elysium-stockiumswap-v3|ethereum-communityv3) echo "ethereum-uniswap-v3-logs-only" ;; \
            robinhood-ekubo-v3|bsc-ekubo-v3|base-ekubo-v3) echo "ethereum-ekubo-v3" ;; \
            robinhood-up-v3) echo "base-aerodrome-slipstreams" ;; \
            elysium-aero-cl) echo "evm-aero-cl" ;; \
            base-balancer-v3|arbitrum-balancer-v3|gnosis-balancer-v3) echo "ethereum-balancer-v3" ;; \
            arc-uniswap-v2) echo "ethereum-uniswap-v2" ;; \
            arc-uniswap-v3) echo "ethereum-uniswap-v3-logs-only" ;; \
            arc-uniswap-v4-no-hooks) echo "ethereum-uniswap-v4" ;; \
            bsc-ring-swap-v2) echo "ethereum-ring-swap-v2" ;; \
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
                for dep in $(sed -n 's#^ *- \.\./\([A-Za-z0-9_-]*\)/proto *$#\1#p' \
                        "/source/$base_dir"/*.yaml | sort -u); do \
                    echo "Including $dep (its protos are imported by $base_dir)..."; \
                    cp -r "/source/$dep" "/filtered/"; \
                done; \
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

RUN apt-get update && apt-get install -y ca-certificates curl libssl3 libpq5 postgresql-client && \
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
RUN mkdir -p /app/adapter-integration/evm

# Copy proto files (needed for `substreams pack`). Packages live at /app/substreams/<pkg> and their
# substreams.yaml uses `importPaths: ../../../proto`, which from there resolves to /proto (the repo
# layout is protocols/substreams/<pkg>, one level deeper). So the tycho protos must sit at /proto.
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/proto /proto

# The EVM project, buildable: the runner builds each package's VM adapter with its own
# constructor arguments, and a strict package's balance lens, with `forge script` at run time
# (scripts/buildRuntime.sh), so the sources, libraries and Foundry config ship with the image.
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/adapter-integration/evm/src /app/adapter-integration/evm/src
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/adapter-integration/evm/lib /app/adapter-integration/evm/lib
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/adapter-integration/evm/scripts /app/adapter-integration/evm/scripts
COPY --from=protocol-sdk-builder /build/tycho-protocol-sdk/protocols/adapter-integration/evm/foundry.toml /build/tycho-protocol-sdk/protocols/adapter-integration/evm/remappings.txt /app/adapter-integration/evm/
COPY --from=tools /out/forge /out/cast /usr/local/bin/
# Compiled here, at the path the runner uses, so the compiler is installed in the image and the
# compile cache holds when a test builds an adapter or a lens.
RUN cd /app/adapter-integration/evm && forge build

# Copy filtered substreams from filter stage
COPY --from=substreams-filter /filtered /app/substreams

# Clean up unnecessary files to reduce size: the substreams' Rust sources and build leftovers. The
# EVM project above keeps its `src`, which the runner builds from.
RUN find /app/substreams -name "*.rs" -delete && \
    find /app/substreams -name "Cargo.toml" -delete && \
    find /app/substreams -name "Cargo.lock" -delete && \
    find /app/substreams -name "src" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app/substreams -name "*.d" -delete && \
    find /app/substreams -name "*.rlib" -delete && \
    find /app/substreams -name "*.rmeta" -delete && \
    find /app/substreams -name ".fingerprint" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app/substreams -name "build" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app/substreams -name "deps" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app/substreams -name "incremental" -type d -exec rm -rf {} + 2>/dev/null || true && \
    find /app/substreams -type d -empty -delete 2>/dev/null || true

# Copy external tools
COPY --from=substreams-cli /app/substreams /usr/local/bin/substreams
RUN chmod +x /usr/local/bin/substreams

# Strip binaries to reduce size
RUN strip /usr/local/bin/* 2>/dev/null || true

WORKDIR /app
ENTRYPOINT ["/entrypoint.sh"]
