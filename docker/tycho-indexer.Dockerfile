# Multi-stage build for tycho-indexer in the monorepo workspace.

# Tools pinned in mise.toml, installed from mise.lock: mise refuses any version not locked there.
FROM jdxcode/mise:2026.10.25-debian@sha256:00a8a9d05a300e5a15a89d6d3a87ea88ec4e78c4702b78d49aa1d3250af61427 AS tools
WORKDIR /tools
COPY mise.toml mise.lock ./
ENV MISE_TRUSTED_CONFIG_PATHS=/tools
RUN mise install github:LukeMathWalker/cargo-chef \
    && mkdir /out \
    && cp "$(mise which cargo-chef)" /out/

# ── Stage 1: chef (rust + tools layer, cached) ─────────────────────────────
FROM rust:1.99.1-bookworm AS chef
WORKDIR /build
RUN apt-get update && apt-get install -y libpq-dev && rm -rf /var/lib/apt/lists/*
COPY --from=tools /out/cargo-chef /usr/local/cargo/bin/
COPY rust-toolchain.toml .
RUN rustup set profile minimal && rustup toolchain install

# ── Stage 2: dependency planner ─────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: build ───────────────────────────────────────────────────────────
FROM chef AS builder
ARG CARGO_PROFILE=release
ARG EXTRA_CARGO_FLAGS=""
COPY --from=planner /build/recipe.json recipe.json
# Pre-build deps only (cached layer)
RUN cargo chef cook --package tycho-indexer --profile ${CARGO_PROFILE} --recipe-path recipe.json
# Full build
COPY . .
RUN RUSTFLAGS="--cfg tokio_unstable" cargo build --package tycho-indexer --profile ${CARGO_PROFILE} ${EXTRA_CARGO_FLAGS}

# ── Stage 4: minimal runtime ─────────────────────────────────────────────────
FROM debian:bookworm-slim
ARG CARGO_PROFILE=release
WORKDIR /opt/tycho-indexer
COPY --from=builder /build/target/${CARGO_PROFILE}/tycho-indexer ./tycho-indexer
COPY crates/tycho-indexer/extractors.yaml ./extractors.yaml
RUN apt-get update && apt-get install -y libpq5 libcurl4 ca-certificates && \
    rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["/opt/tycho-indexer/tycho-indexer", \
            "--endpoint", "https://mainnet.eth.streamingfast.io:443", \
            "index"]
