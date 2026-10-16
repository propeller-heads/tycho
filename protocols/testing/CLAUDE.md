# protocols/testing/

Rust binary (`protocol-testing`) that runs end-to-end integration tests for Substreams protocol
implementations. Spins up a full indexer stack, indexes a block range from each protocol's
`integration_test.tycho.yaml`, then validates resulting state via `tycho-simulation`.

IS in the monorepo `[workspace.members]` (unlike `protocols/substreams/`).

## Running

Requires: `RPC_URL`, `SUBSTREAMS_API_TOKEN`, Postgres (`DATABASE_URL`, default
`postgres://postgres:mypassword@localhost:5431/tycho_indexer_0`).

```bash
cargo run -- range --package "ethereum-balancer-v2"          # block range from yaml
cargo run -- full  --package "ethereum-balancer-v2"          # creation block to latest
cargo run -- range --package "base-aerodrome-slipstreams" --chain base
```

WARNING: each run drops and recreates the database named in `DATABASE_URL`. The spawned
tycho-indexer listens on `TYCHO_SERVER_PORT` / `--tycho-server-port` (default 4242). To run
alongside another Tycho stack, isolate both the database and the port — see `README.md`
("Running alongside another Tycho stack"). The compose db host port is `DB_HOST_PORT`
(default 5431).

Docker Compose is available for isolated runs — see `README.md`.

## Package aliases and CI

`protocol_packages.json` is the shared fork-to-package mapping used by the Rust runner, Docker
build/filter stages and CI. Add aliases there rather than editing Dockerfile cases or Rust tables.
Nested values name the runtime directory; builds/copies use its top-level workspace.

`scripts/select_protocols.py` runs from the trusted PR base checkout, reading head files only as
data. Mapping and alias manifest/fixture changes select their owners; shared package files select
all aliases of the workspace; non-Markdown testing infrastructure changes select all eligible
packages. Root workspace dependency/core changes use manual dispatch. Regression tests run in a
separate `pull_request` workflow, without the integration workflow's base-branch context or caches.
