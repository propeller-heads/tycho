# Multi-stage build for tycho-indexer in the monorepo workspace.

# ── Stage 1: chef (rust + tools layer, cached) ─────────────────────────────
FROM rust:1.91-bookworm AS chef
WORKDIR /build
RUN apt-get update && apt-get install -y libpq-dev && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef
COPY rust-toolchain.toml .
RUN rustup set profile minimal && rustup show

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
