# Ships substreams-sink-sql with the SQL and scripts it needs.
#
# The .spkg packages are NOT built here. release.sh publishes them to S3 and each sink container
# fetches the one it is pinned to, so this image carries no module hash: rebuilding it, for any
# reason, cannot change what a sink reads or invalidate a cursor.
#
# Build from the repository root:
#   docker build -f crates/tycho-execution/substreams/Dockerfile -t tycho-router-trades .
# Tools pinned in mise.toml, installed from mise.lock: mise refuses any version not locked there.
FROM jdxcode/mise:2026.10.25-debian@sha256:00a8a9d05a300e5a15a89d6d3a87ea88ec4e78c4702b78d49aa1d3250af61427 AS tools
WORKDIR /tools
COPY mise.toml mise.lock ./
ENV MISE_TRUSTED_CONFIG_PATHS=/tools
RUN mise install github:streamingfast/substreams-sink-sql \
    && mkdir /out \
    && cp "$(mise which substreams-sink-sql)" /out/

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl postgresql-client awscli \
    && apt-get clean
COPY --from=tools /out/substreams-sink-sql /usr/local/bin/
WORKDIR /opt/router-trades
COPY crates/tycho-execution/substreams/schema.sql crates/tycho-execution/substreams/executors.sql crates/tycho-execution/substreams/views.sql ./
COPY crates/tycho-execution/substreams/pricing ./pricing
COPY crates/tycho-execution/substreams/scripts/price_trades.sh crates/tycho-execution/substreams/scripts/fdw_setup.sh ./scripts/
COPY crates/tycho-execution/substreams/docker/entrypoint.sh /usr/local/bin/entrypoint
RUN chmod +x /usr/local/bin/entrypoint ./scripts/*.sh
ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["sink"]
