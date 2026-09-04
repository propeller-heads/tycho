# Builds the wasm module, packs one .spkg per chain and ships them with substreams-sink-sql.
# Build from the repository root:
#   docker build -f crates/tycho-execution/substreams/Dockerfile -t tycho-router-trades .
ARG SUBSTREAMS_VERSION=v1.22.0
ARG SINK_SQL_VERSION=v4.13.1

FROM rust:1.91-bookworm AS build
ARG SUBSTREAMS_VERSION
ARG TARGETARCH=amd64
RUN case "$TARGETARCH" in amd64) ARCH=x86_64 ;; arm64) ARCH=arm64 ;; *) echo "unsupported arch $TARGETARCH" && exit 1 ;; esac \
    && curl -fsSL --retry 5 --retry-all-errors -o /tmp/substreams.tgz \
       "https://github.com/streamingfast/substreams/releases/download/${SUBSTREAMS_VERSION}/substreams_linux_${ARCH}.tar.gz" \
    && tar -xzf /tmp/substreams.tgz -C /usr/local/bin substreams && rm /tmp/substreams.tgz
WORKDIR /build
COPY crates/tycho-execution/substreams/rust-toolchain.toml .
RUN rustup set profile minimal && rustup show
COPY crates/tycho-execution/substreams/ .
RUN cargo build --target wasm32-unknown-unknown --release --target-dir target
RUN mkdir -p /spkg && for manifest in tycho-router-trades/chains/*.yaml; do \
        chain=$(basename "$manifest" .yaml); \
        substreams pack "$manifest" -o "/spkg/${chain}.spkg"; \
    done

FROM debian:bookworm-slim
ARG SINK_SQL_VERSION
ARG TARGETARCH=amd64
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl postgresql-client \
    && apt-get clean \
    && case "$TARGETARCH" in amd64) ARCH=x86_64 ;; arm64) ARCH=arm64 ;; esac \
    && curl -fsSL --retry 5 --retry-all-errors -o /tmp/sink.tgz \
       "https://github.com/streamingfast/substreams-sink-sql/releases/download/${SINK_SQL_VERSION}/substreams-sink-sql_linux_${ARCH}.tar.gz" \
    && tar -xzf /tmp/sink.tgz -C /usr/local/bin substreams-sink-sql && rm /tmp/sink.tgz
WORKDIR /opt/router-trades
COPY --from=build /spkg ./spkg
COPY crates/tycho-execution/substreams/schema.sql ./
COPY crates/tycho-execution/substreams/pricing ./pricing
COPY crates/tycho-execution/substreams/scripts/price_trades.sh crates/tycho-execution/substreams/scripts/fdw_setup.sh ./scripts/
COPY crates/tycho-execution/substreams/docker/entrypoint.sh /usr/local/bin/entrypoint
RUN chmod +x /usr/local/bin/entrypoint ./scripts/*.sh
ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["sink"]
