# Ships substreams-sink-sql with the SQL and scripts it needs.
#
# The .spkg packages are NOT built here. release.sh publishes them to S3 and each sink container
# fetches the one it is pinned to, so this image carries no module hash: rebuilding it, for any
# reason, cannot change what a sink reads or invalidate a cursor.
#
# Build from the repository root:
#   docker build -f crates/tycho-execution/substreams/Dockerfile -t tycho-router-trades .
ARG SINK_SQL_VERSION=v4.13.1

FROM debian:bookworm-slim
ARG SINK_SQL_VERSION
# Kaniko does not set TARGETARCH, so it needs a default.
ARG TARGETARCH=amd64
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl postgresql-client awscli \
    && apt-get clean \
    && case "$TARGETARCH" in amd64) ARCH=x86_64 ;; arm64) ARCH=arm64 ;; *) echo "unsupported arch $TARGETARCH" && exit 1 ;; esac \
    && curl -fsSL --retry 5 --retry-all-errors -o /tmp/sink.tgz \
       "https://github.com/streamingfast/substreams-sink-sql/releases/download/${SINK_SQL_VERSION}/substreams-sink-sql_linux_${ARCH}.tar.gz" \
    && tar -xzf /tmp/sink.tgz -C /usr/local/bin substreams-sink-sql && rm /tmp/sink.tgz
WORKDIR /opt/router-trades
COPY crates/tycho-execution/substreams/schema.sql ./
COPY crates/tycho-execution/substreams/pricing ./pricing
COPY crates/tycho-execution/substreams/scripts/price_trades.sh crates/tycho-execution/substreams/scripts/fdw_setup.sh ./scripts/
COPY crates/tycho-execution/substreams/docker/entrypoint.sh /usr/local/bin/entrypoint
RUN chmod +x /usr/local/bin/entrypoint ./scripts/*.sh
ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["sink"]
