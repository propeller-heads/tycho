# Tools pinned in mise.toml, installed from mise.lock: mise refuses any version not locked there.
FROM jdxcode/mise:2026.10.25-debian@sha256:00a8a9d05a300e5a15a89d6d3a87ea88ec4e78c4702b78d49aa1d3250af61427 AS tools
WORKDIR /tools
COPY mise.toml mise.lock ./
ENV MISE_TRUSTED_CONFIG_PATHS=/tools
RUN mise install github:streamingfast/substreams github:LukeMathWalker/cargo-chef \
    && mkdir /out \
    && cp "$(mise which substreams)" "$(mise which cargo-chef)" /out/

FROM rust:1.99.1-bookworm AS chef
WORKDIR /build
RUN apt-get update && apt-get install -y libpq-dev
COPY --from=tools /out/substreams /usr/local/bin/
COPY --from=tools /out/cargo-chef /usr/local/cargo/bin/
COPY rust-toolchain.toml .
RUN rustup set profile minimal && rustup install

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --package tycho-indexer --release --recipe-path recipe.json
COPY . .
RUN ./stable-build.sh

FROM debian:bookworm
WORKDIR /opt/tycho-indexer
COPY --from=build /build/target/release/tycho-indexer ./tycho-indexer
COPY crates/tycho-indexer/extractors.yaml ./extractors.yaml
RUN apt-get update && apt-get install -y libpq-dev libcurl4 && rm -rf /var/lib/apt/lists/*
ENTRYPOINT [ "/opt/tycho-indexer/tycho-indexer", "--endpoint", "https://mainnet.eth.streamingfast.io:443", "index"]
