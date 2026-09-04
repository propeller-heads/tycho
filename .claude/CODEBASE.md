<!-- docs-synced-at: 39b44a1b29eeb5fc1b6a8f5da0c9684fa78b39d6 -->
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
| [`tycho-common`](../crates/tycho-common/CLAUDE.md) | Domain types (`Chain`, `Block`, `ProtocolComponent`, `Token`), DTOs, async gateway/extraction traits, simulation abstractions (`SwapQuoter`), custom-chain registry (`models/chain_config.rs`) |
| `tycho-protobuf` | Substreams protobuf bindings (`pb/`) plus the `TryFromMessage` conversions that decode them into `tycho-common` models; consumed by `tycho-indexer` |
| `tycho` | Meta-crate re-exporting a compatible, versioned set of ecosystem crates for downstream consumers |

**Features on `tycho-common`**: `diesel` (Diesel derives), `test-utils` (mockall mocks).

### Indexer

| Crate / Module | Description |
|---|---|
| [`tycho-indexer/extractor`](../crates/tycho-indexer/CLAUDE.md) | `ProtocolExtractor` processes Substreams messages, `ReorgBuffer` handles finality, `ProtocolMemoryCache` for in-process state, DCI plugin for VM tracing |
| [`tycho-indexer/services`](../crates/tycho-indexer/CLAUDE.md) | HTTP RPC endpoints, WebSocket broadcaster, `PendingDeltasBuffer` for RPC consistency, access control, plan restrictions, compression |
| [`tycho-ethereum`](../crates/tycho-ethereum/CLAUDE.md) | Ethereum RPC client (alloy), `AccountExtractor`, `TokenPreProcessor`, `TokenAnalyzer`, `EntryPointTracer`, `BalanceSlotDetector`/`AllowanceSlotDetector`, `FeePriceGetter` |
| [`tycho-storage`](../crates/tycho-storage/CLAUDE.md) | Postgres backend (Diesel): `CachedGateway` (buffered writes), `DirectGateway` (testing), temporal versioning, FK-safe write ordering |

### Simulation & Execution

| Crate | Description |
|---|---|
| `tycho-simulation` | DEX swap simulation library: protocol-specific state machines (`ProtocolSim`) for 20+ DEXs; all protocol implementations live under `evm/protocol/`, `rfq` for request-for-quote protocols, `price_level_stream` for the Titan pAMM price level stream |
| `tycho-execution` | Swap encoding and execution: Solidity TychoRouterV3 contract + Rust encoding library; multi-hop swaps with fee-taking, vault-based accounting, delegatecall executor dispatch |
| `tycho-execution/model` (`tycho-router-model`) | Rust security model of the caller-controlled executors, used to reason about router invariants |

### Consumer SDK

| Crate | Description |
|---|---|
| [`tycho-client`](../crates/tycho-client/CLAUDE.md) | Rust library + CLI: `TychoStreamBuilder`, snapshot+delta sync, block alignment across extractors, TVL/ID filtering |
| `tycho-client-py` | Python bindings (maturin/PyO3) wrapping tycho-client (separate workspace, not a `[workspace.members]` entry) |

### Protocols

| Path | Description |
|---|---|
| `protocols/substreams/` | Substreams modules (WASM) producing the protobuf messages consumed by `tycho-indexer`; **separate WASM workspace** with its own toolchain — not in `[workspace.members]` |
| `crates/tycho-execution/substreams/` | TychoRouter trade indexer: a **second, separate WASM workspace** (excluded from the root `Cargo.toml`) that decodes every router trade from call traces and sinks it to Postgres with `substreams-sink-sql`, priced in USD afterwards. Adding a chain, or changing a manifest or its Rust source, has deployment consequences — see `crates/tycho-execution/CLAUDE.md` and `crates/tycho-execution/substreams/README.md` |
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
2. `ProtocolExtractor` deserializes into `BlockChanges` (tx-level state/balance/token deltas) via
   `tycho-protobuf`'s `TryFromMessage` conversions
   - `PartialBlockBuffer` accumulates sub-block messages until full-block signal arrives
   - `TokenPreProcessor` fetches metadata (symbol, decimals) via Ethereum RPC for unknown tokens
3. `BlockChanges` inserted into `ReorgBuffer` (one per `ProtocolExtractor`)
   - On `BlockUndoSignal`: purge blocks after the reverted hash (falling back to the target height when the hash is unknown), emit revert messages — no DB rollback needed
   - Drain to DB when `count_blocks_before(finalized_block_height) >= commit_batch_size` — only finalized blocks ever reach DB
4. Drained blocks: `BlockChanges` → `BlockAggregatedChanges` (merge all tx-level deltas into one state per component/account)
5. DB write via `CachedGateway` → Postgres (upsert blocks, tokens, components, state, balances); sets `db_committed_block_height` on outgoing message
6. Broadcast a `DeltaCommand` on the internal channel — `Block(BlockAggregatedChanges)` for all
   blocks (including pending/non-committed), or `ExtractorRestarted` when the supervisor rebuilt
   the extractor

### Server

7. WebSocket subscribers (`services/ws.rs`) receive broadcast directly; revert flag signals chain
   reorg. On `ExtractorRestarted`, `ws.rs` sends `Response::SubscriptionEnded` and `PendingDeltas`
   resets that extractor's buffer
8. `PendingDeltasBuffer` (`services/deltas_buffer.rs`) receives broadcast
   - Inserts every full block (partial blocks skipped)
   - Auto-drains blocks ≤ `db_committed_block_height` (already in DB, no longer "pending")
   - RPC handlers query DB snapshot + pending deltas = consistent view of latest state

### Client (tycho-client)

9. `StateSynchronizer` (one per extractor subscription):
   - Subscribes to WebSocket stream via `WsDeltasClient`
   - Discards WebSocket messages that arrive before the sync loop starts, then fetches the HTTP
     snapshot via `HttpRPCClient` **synchronously** at the first message's block height
   - Components discovered later are snapshotted in background tasks and their deltas buffered
     until the snapshot lands, so the delta loop never blocks on RPC
10. `BlockSynchronizer` (across all extractors):
    - Tracks state per synchronizer: `Started` / `Ready` / `Delayed` / `Stale` / `Advanced` / `Ended`
    - Delayed synchronizers consume buffered messages to catch up
    - When all synchronizers reach the same block: emits `FeedMessage` (unified view of all protocol state at that block) to consumer

### Simulation & Execution (tycho-simulation / tycho-execution)

11. Consumer applies `FeedMessage` deltas to in-memory `ProtocolSim` instances (one per component)
    - Custom protocols: update decoded state fields directly
    - VM protocols: patch EVM storage slots, code, balances in a local `SimulationDB`
12. Consumer queries `ProtocolSim::get_amount_out` / `spot_price` to price swap routes
13. `tycho-execution` encodes a chosen route (`Solution`) into an `EncodedSolution` for `TychoRouterV3`
    - Selects the appropriate executor contract for each DEX hop
    - Batches consecutive hops on the same groupable protocol into `SwapGroup`s
14. Consumer submits the encoded transaction to one of the router's 9 entry points —
    `{single,sequential,split}Swap` × `{plain, Permit2, UsingVault}`

## Key Architectural Patterns

### Extractor-per-protocol

Each protocol is owned by an `ExtractorSupervisor` tokio task, which builds an extractor +
`ExtractorRunner` via `ExtractorFactory` and rebuilds them with exponential backoff on failure.
Config in `extractors.yaml`. Extractors are stateful: they track components, state history, and
Substreams cursor.

### Implementation types: Custom, VM, and Hybrid

The `ImplementationType` enum has two variants (`Custom`, `Vm`), but three patterns exist:

- **Custom**: State fully described by Substreams output (reserves, fees). Uniswap V2/V3.
- **VM**: Requires full contract storage for simulation. Tracks code, storage slots, balances.
  Balancer V2, Maverick V2. Often paired with DCI for dynamic contract discovery.
- **Hybrid**: Combines explicit protocol state attributes (from Substreams) with contract storage
  tracking. Simulation uses both: decoded state for known fields, raw storage for on-chain VM
  execution. Runtime pattern, not a separate enum variant. Fluid V1, Balancer V3, Curve. Note that
  Balancer V3 and Curve keep VM-shaped indexing (`vm:` component keys, full contract storage) —
  only their quote path is native Rust.

### ReorgBuffer + finality

Blocks enter `ReorgBuffer` immediately but only reach the DB via
`drain_blocks_until(finalized_height)`. The DB always reflects canonical chain state. Pending
blocks are served to RPC via `PendingDeltasBuffer`.

### Temporal versioning

Every mutable Postgres entity carries `valid_from`/`valid_to`. `apply_versioning()` sets
`valid_to` on the previous row when a new version is inserted; protocol state, component balances
and contract storage go through `apply_partitioned_versioning()` instead (partitioned tables).
Historical rows are never mutated, but history is bounded: pg_cron jobs drop expired partitions
(`drop_expired_partitions()`) and prune orphaned transactions
(`cleanup_orphaned_transactions()`), both reading the horizon from the
`partition_retention_config` table (default 1 month).

### Dual runtime

The `index` command runs two tokio runtimes: extraction (CPU-bound) and server/gateway (I/O-bound).
Configurable via `EXTRACTION_WORKER_THREADS` (default 2) and `MAIN_WORKER_THREADS` (default 3).

## Configuration

### Chains

Built-in `Chain` variants cover the chains Tycho officially supports. A self-hosted instance can
add its own via the custom-chain registry (`tycho-common/src/models/chain_config.rs`): a
`chains.yaml` file (`--chain-config` / `TYCHO_CHAINS_CONFIG`, default `./chains.yaml`) is loaded
into a process-global `ChainConfigRegistry` and each entry becomes a `Chain::Custom(CustomChainId)`
carrying its quote tokens and TVL thresholds. **The registry must be installed before any chain
name is parsed** — `Chain::from_str` is registry-backed and fallible, so an unresolvable name is an
error rather than a silent custom chain (`Chain::builtin_from_str` skips the registry).

### Environment variables

| Variable | Purpose |
|---|---|
| `DATABASE_URL` | Postgres connection string |
| `RPC_URL` | Ethereum JSON-RPC endpoint |
| `AUTH_API_KEY` | API key for RPC access control |
| `SUBSTREAMS_API_TOKEN` | Substreams gRPC auth |
| `TYCHO_CHAINS_CONFIG` | Custom-chain registry YAML (default `./chains.yaml`) |
| `EXTRACTORS_CONFIG` | Extractor config YAML (default `./extractors.yaml`) |
| `RETENTION_HORIZON` | Datetime before which data is not kept |
| `EXTRACTION_WORKER_THREADS` | Extraction runtime threads (default 2) |
| `MAIN_WORKER_THREADS` | Server runtime threads (default 3) |
| `RPC_MAX_RETRIES` / `RPC_INITIAL_BACKOFF_MS` / `RPC_MAX_BACKOFF_MS` | RPC retry policy |
| `RPC_MAX_BATCH_SIZE` / `RPC_STORAGE_SLOT_MAX_BATCH_SIZE` | RPC request batching limits |
| `TYCHO_S3_BUCKET` | S3 bucket the Substreams spkg packages are fetched from |
| `OTLP_EXPORTER_ENDPOINT` | OpenTelemetry trace exporter |
| `RUST_LOG` | Tracing filter (e.g. `tycho_indexer=debug`) |

### CLI commands

| Command | Purpose |
|---|---|
| `index` | Run all extractors from `extractors.yaml` + HTTP/WS server; `--chain-config` for custom chains |
| `run` | Run a single extractor (testing / debugging); `--chain-config` for custom chains |
| `analyze-tokens` | Token quality analysis cron job; accepts `--settlement-contract <ADDRESS>` (default: CoW Swap settlement `0xc9f2e6ea1637E499406986ac50ddC92401ce1f58`) and `--recovery-lookback-days <N>` (default 1: re-check quality-5 tokens traded within N days; Bad keeps 5) |
| `rpc` | HTTP RPC server only (no extractors); takes `--chain` and `--chain-config` |

### Feature flags

| Crate | Feature | Effect |
|---|---|---|
| `tycho-common` | `diesel` | Diesel derives on `Bytes` and model types |
| `tycho-common` | `test-utils` | `mockall` auto-mocks on trait abstractions |
| `tycho-indexer` | `jemalloc` (default) | jemalloc allocator + the `GET /debug/pprof/heap` route |
| `tycho-execution` | `evm` (default) | alloy + reqwest encoding support |
| `tycho-execution` | `fork-tests`, `test-utils` | Mainnet-fork tests; test helpers |
| `tycho-simulation` | `evm`, `rfq`, `price-level-stream` (all default) | See `tycho-simulation/CLAUDE.md` |
| `tycho-simulation` | `network_tests` | Gates tests needing live network access |
| `tycho` | `evm` (default), `rfq` | Which ecosystem crates the meta-crate re-exports |

Everything else is controlled via CLI args, env vars, and YAML config.

## Testing

- `cargo nextest run --workspace --all-features --locked` — CI's test runner
- DB serial tests: name must include `serial_db` (nextest test group, sequential)
- DB harness: `run_against_db` (tycho-storage) manages setup/teardown
- Archive RPC tests: `#[ignore]`-d
- Lint: `cargo +nightly-2026-06-28 clippy --workspace --all-targets --all-features -- -D warnings`
- Format: `cargo +nightly-2026-06-28 fmt --all --check`
- Also gated in CI: `cargo doc --workspace --no-deps --all-features --locked` and
  `cargo check --workspace --no-default-features --locked`

Exact commands and the nextest filter expressions live in `.github/workflows/ci-rust.yaml`; the
`run-ci` skill runs them locally.

