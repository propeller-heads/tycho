# This Dockerfile creates a custom postgres image used for CI and local deployment.
# Includes the extensions: pg_partman, pg_cron.

# Stage 1: Build pg_cron extension
# Base image pinned by digest (tag 15-5 is mutable and drifted Alpine versions, breaking the clang/llvm pins).
FROM ghcr.io/dbsystel/postgresql-partman:15-5@sha256:1c5f452535daf6060be8ddd95d42062c9fa212a1cd772648579098821ad486f3 AS builder
ARG PGCRON_VERSION="1.6.2"
USER root

# clang/llvm version must match the postgres base image's PGXS (it invokes clang-21 for JIT bitcode).
RUN apk update && apk add --no-cache build-base clang21 llvm21 wget

RUN cd /tmp \
    && wget "https://github.com/citusdata/pg_cron/archive/refs/tags/v${PGCRON_VERSION}.tar.gz" \
    && tar zxf v${PGCRON_VERSION}.tar.gz \
    && cd pg_cron-${PGCRON_VERSION} \
    && make \
    && make install \
    && cd .. && rm -r pg_cron-${PGCRON_VERSION} v${PGCRON_VERSION}.tar.gz

# Stage 2: Final image, copy built extension
FROM ghcr.io/dbsystel/postgresql-partman:15-5@sha256:1c5f452535daf6060be8ddd95d42062c9fa212a1cd772648579098821ad486f3
USER root

COPY --from=builder /usr/local/lib/postgresql/pg_cron.so /usr/local/lib/postgresql/
COPY --from=builder /usr/local/share/postgresql/extension/pg_cron* /usr/local/share/postgresql/extension/

USER postgres
