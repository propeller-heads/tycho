<!-- docs-synced-at: 2342f18bda260cfaf2d403d8fdc079d56d7ea476 -->
# Tycho Codebase Guide

Low-latency, reorg-aware indexer that streams DEX liquidity state from on-chain data to consumers.

## What is Tycho

Tycho indexes EVM blockchain state for DeFi protocols. It consumes Substreams (blockchain data
pipelines), processes fork-aware messages through extractors, persists finalized state to Postgres,
and serves real-time deltas over WebSocket plus snapshots over HTTP RPC. Consumers reconstruct
protocol component state for simulation, pricing, and execution.

Key properties:
- **Reorg-aware**: blocks stay in a memory buffer until finalized; reverts never reach the DB
- **Real-time**: WebSocket subscribers receive deltas within the same block they're processed
- **Temporal**: every mutable DB row carries `valid_from`/`valid_to` for time-travel queries
- **Multi-protocol**: a single instance runs N extractors concurrently (one per protocol)

## Workspace Module Map

The monorepo is organized into four layers: **Foundation** (shared types), **Indexer** (on-chain data
pipeline), **Simulation & Execution** (solver tooling), and **Consumer SDK** (connecting to Tycho).
Protocol Substreams modules live under `protocols/` as a separate WASM workspace.

### Foundation

| Crate | Description |
|---|---|
| [`tycho-common`](../crates/tycho-common/CLAUDE.md) | Domain types (`Chain`, `Block`, `ProtocolComponent`, `Token`), DTOs, async gateway/extraction traits, simulation abstractions (`SwapQuoter`) |
| `tycho` | Meta-crate re-exporting a compatible, versioned set of ecosystem crates for downstream consumers |

**Features on `tycho-common`**: `diesel` (Diesel derives), `test-utils` (mockall mocks).

### Indexer

| Crate / Module | Description |
|---|---|
| [`tycho-indexer/extractor`](../crates/tycho-indexer/CLAUDE.md) | `ProtocolExtractor` processes Substreams messages, `ReorgBuffer` handles finality, `ProtocolMemoryCache` for in-process state, DCI plugin for VM tracing |
| [`tycho-indexer/services`](../crates/tycho-indexer/CLAUDE.md) | HTTP RPC endpoints, WebSocket broadcaster, `PendingDeltasBuffer` for RPC consistency, access control, plan restrictions, compression |
| [`tycho-ethereum`](../crates/tycho-ethereum/CLAUDE.md) | Ethereum RPC client (alloy), `AccountExtractor`, `TokenPreProcessor`, `TokenAnalyzer`, `EntryPointTracer` |
| [`tycho-storage`](../crates/tycho-storage/CLAUDE.md) | Postgres backend (Diesel): `CachedGateway` (buffered writes), `DirectGateway` (testing), temporal versioning, FK-safe write ordering |

### Simulation & Execution

| Crate | Description |
|---|---|
| `tycho-simulation` | DEX swap simulation library: protocol-specific state machines (`ProtocolSim`) for 20+ DEXs; `evm` module for EVM storage-based protocols, `protocol` module for custom implementations, `rfq` for request-for-quote protocols |
| `tycho-execution` | Swap encoding and execution: Solidity TychoRouter contract + Rust encoding library; multi-hop swaps with fee-taking, vault-based accounting, delegatecall executor dispatch |

### Consumer SDK

| Crate | Description |
|---|---|
| [`tycho-client`](../crates/tycho-client/CLAUDE.md) | Rust library + CLI: `TychoStreamBuilder`, snapshot+delta sync, block alignment across extractors, TVL/ID filtering |
| `tycho-client-py` | Python bindings (maturin/PyO3) wrapping tycho-client (separate workspace, not a `[workspace.members]` entry) |

### Protocols

| Path | Description |
|---|---|
| `protocols/substreams/` | Substreams modules (WASM) producing the protobuf messages consumed by `tycho-indexer`; **separate WASM workspace** with its own toolchain — not in `[workspace.members]` |
| `protocols/testing/` (`protocol-testing`) | Simulation accuracy test harness: runs protocol state through `tycho-simulation` and compares against on-chain results |
| `protocols/adapter-integration/` | EVM adapter integration tests |

### Testing Infrastructure

| Crate | Description |
|---|---|
| `tycho-test` | Shared test helpers and fixtures used across crates |
| `tycho-integration-test` | End-to-end integration runner: subscribes to a live Tycho instance, syncs protocol state via `tycho-client`, and validates simulation accuracy against on-chain prices |

## End-to-End Data Flow

### Ingestion

1. Substreams gRPC delivers `BlockScopedData` (protobuf) to `ProtocolExtractor`
2. `ProtocolExtractor` deserializes into `BlockChanges` (tx-level state/balance/token deltas)
   - `PartialBlockBuffer` accumulates sub-block messages until full-block signal arrives
   - `TokenPreProcessor` fetches metadata (symbol, decimals) via Ethereum RPC for unknown tokens
3. `BlockChanges` inserted into `ReorgBuffer` (one per `ProtocolExtractor`)
   - On `BlockUndoSignal`: purge blocks after reverted hash, emit revert messages — no DB rollback needed
   - Drain to DB when `count_blocks_before(finalized_block_height) >= commit_batch_size` — only finalized blocks ever reach DB
4. Drained blocks: `BlockChanges` → `BlockAggregatedChanges` (merge all tx-level deltas into one state per component/account)
5. DB write via `CachedGateway` → Postgres (upsert blocks, tokens, components, state, balances); sets `db_committed_block_height` on outgoing message
6. Broadcast `BlockAggregatedChanges` on internal channel (all blocks, including pending/non-committed)

### Server

7. WebSocket subscribers (`services/ws.rs`) receive broadcast directly; revert flag signals chain reorg
8. `PendingDeltasBuffer` (`services/deltas_buffer.rs`) receives broadcast
   - Inserts every full block (partial blocks skipped)
   - Auto-drains blocks ≤ `db_committed_block_height` (already in DB, no longer "pending")
   - RPC handlers query DB snapshot + pending deltas = consistent view of latest state

### Client (tycho-client)

9. `StateSynchronizer` (one per extractor subscription):
   - Subscribes to WebSocket stream via `WsDeltasClient`
   - On first message: fetches HTTP snapshot via `HttpRPCClient` at that block height
   - Buffers WebSocket deltas received before snapshot arrives; applies them on top to catch up
10. `BlockSynchronizer` (across all extractors):
    - Tracks state per synchronizer: `Ready` / `Delayed` / `Stale`
    - Delayed synchronizers consume buffered messages to catch up
    - When all synchronizers reach the same block: emits `FeedMessage` (unified view of all protocol state at that block) to consumer

### Simulation & Execution (tycho-simulation / tycho-execution)

11. Consumer applies `FeedMessage` deltas to in-memory `ProtocolSim` instances (one per component)
    - Custom protocols: update decoded state fields directly
    - VM protocols: patch EVM storage slots, code, balances in a local `SimulationDB`
12. Consumer queries `ProtocolSim::get_amount_out` / `spot_price` to price swap routes
13. `tycho-execution` encodes a chosen route into calldata for `TychoRouter`
    - Selects the appropriate executor contract for each DEX hop
    - Constructs `SwapSequence` with per-hop amounts, tokens, and executor addresses
14. Consumer submits the encoded transaction to the chain via `TychoRouter.swap()`

## Key Architectural Patterns

### Extractor-per-protocol

Each protocol runs as a separate `ExtractorRunner` tokio task. Config in `extractors.yaml`.
Extractors are stateful: they track components, state history, and Substreams cursor.

### Implementation types: Custom, VM, and Hybrid

The `ImplementationType` enum has two variants (`Custom`, `Vm`), but three patterns exist:

- **Custom**: State fully described by Substreams output (reserves, fees). Uniswap V2/V3.
- **VM**: Requires full contract storage for simulation. Tracks code, storage slots, balances.
  Balancer V2, Curve. Often paired with DCI for dynamic contract discovery.
- **Hybrid**: Combines explicit protocol state attributes (from Substreams) with contract storage
  tracking. Simulation uses both: decoded state for known fields, raw storage for on-chain VM
  execution. Runtime pattern, not a separate enum variant.

### ReorgBuffer + finality

Blocks enter `ReorgBuffer` immediately but only reach the DB via
`drain_blocks_until(finalized_height)`. The DB always reflects canonical chain state. Pending
blocks are served to RPC via `PendingDeltasBuffer`.

### Temporal versioning

Every mutable Postgres entity carries `valid_from`/`valid_to`. `apply_versioning()` sets
`valid_to` on the previous row when a new version is inserted. Historical rows are never mutated.

### Dual runtime

The `index` command runs two tokio runtimes: extraction (CPU-bound) and server/gateway (I/O-bound).
Configurable via `EXTRACTION_WORKER_THREADS` (default 2) and `MAIN_WORKER_THREADS` (default 3).

## Configuration

### Environment variables

| Variable | Purpose |
|---|---|
| `DATABASE_URL` | Postgres connection string |
| `RPC_URL` | Ethereum JSON-RPC endpoint |
| `AUTH_API_KEY` | API key for RPC access control |
| `SUBSTREAMS_API_TOKEN` | Substreams gRPC auth |
| `EXTRACTION_WORKER_THREADS` | Extraction runtime threads (default 2) |
| `MAIN_WORKER_THREADS` | Server runtime threads (default 3) |
| `OTLP_EXPORTER_ENDPOINT` | OpenTelemetry trace exporter |
| `RUST_LOG` | Tracing filter (e.g. `tycho_indexer=debug`) |

### CLI commands

| Command | Purpose |
|---|---|
| `index` | Run all extractors from `extractors.yaml` + HTTP/WS server |
| `run` | Run a single extractor (testing / debugging) |
| `analyze-tokens` | Token quality analysis cron job; accepts `--settlement-contract <ADDRESS>` (default: CoW Swap settlement `0xc9f2e6ea1637E499406986ac50ddC92401ce1f58`) |
| `rpc` | HTTP RPC server only (no extractors) |

### Feature flags

| Crate | Feature | Effect |
|---|---|---|
| `tycho-common` | `diesel` | Diesel derives on `Bytes` and model types |
| `tycho-common` | `test-utils` | `mockall` auto-mocks on trait abstractions |

No other crate-level features. Runtime behavior controlled via CLI args, env vars, and YAML config.

## Testing

- `cargo test` for standard tests
- DB serial tests: name must include `serial_db` (nextest test group, sequential)
- DB harness: `run_against_db` (tycho-storage) manages setup/teardown
- Archive RPC tests: `#[ignore]`-d
- Lint: `cargo clippy --workspace --lib --all-targets --all-features`
- Format: `cargo +nightly fmt --check`

