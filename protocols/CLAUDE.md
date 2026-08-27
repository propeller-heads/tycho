# protocols/

On-chain protocol indexing: Substreams modules that extract state from the chain, and the
tooling to test and validate those integrations. Each sub-directory has its own toolchain
and workspace — see the relevant `CLAUDE.md` for details.

| Directory | Description |
|---|---|
| [`substreams/`](substreams/CLAUDE.md) | WASM modules extracting on-chain state → protobuf for `tycho-indexer` |
| `crates/` | Native processors implementing `TxDeltaIndexer`, for pending-block simulation without a Substreams runtime. Root `[workspace.members]` entries, one per protocol |
| [`testing/`](testing/CLAUDE.md) | End-to-end integration test runner for Substreams implementations |
| [`adapter-integration/`](adapter-integration/CLAUDE.md) | Foundry fork tests for `tycho-execution` VM adapter contracts |
