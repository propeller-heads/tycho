# tycho-indexer

Main indexing engine: connects to Substreams, processes block data, persists finalized state, and
serves it over HTTP/WebSocket.

## Module Map

```
main.rs                     CLI entry point; account initialisation; extractor startup
cli.rs                      Command definitions: Index, Run, AnalyzeTokens, Rpc
ot.rs                       OpenTelemetry tracing configuration
testing.rs                  Test utilities

extractor/
  protocol_extractor.rs     ProtocolExtractor — core message processor (see below)
  runner.rs                 ExtractorRunner: drives the Substreams stream; ExtractorHandle for control
  reorg_buffer.rs           ReorgBuffer — finality-aware block queue; chain-reorg purge
  models.rs                 BlockChanges, BlockAggregatedChanges, TxWithChanges
  protocol_cache.rs         ProtocolMemoryCache — in-process token/component metadata cache
  chain_state.rs            ChainState — tracks current tip and finality horizon
  u256_num.rs               U256 numeric utilities
  token_analysis_cron.rs    Background job: token quality / tax analysis
  dynamic_contract_indexer/ DCI optional extension (see below)
    dci.rs                  Core DCI: DynamicContractIndexer implementation
    cache.rs                DCI component/contract cache
    hooks/                  UniswapV4-style hooks DCI variant
      hook_dci.rs           HooksDCI — hooks-specific ExtractorExtension
      hooks_dci_builder.rs  Builder for HooksDCI
      hook_orchestrator.rs  Orchestrates hook detection + metadata
      hook_permissions_detector.rs  Detects hook permission flags
      entrypoint_generator.rs      Generates entry points for hook contracts
      component_metadata.rs        Hook component metadata types
      metadata_orchestrator.rs     Coordinates metadata providers
      rpc_metadata_provider.rs     Fetches hook metadata via RPC
      integrations/         Protocol-specific hook metadata (e.g. euler/)
  post_processors/          Optional block-level post-processing hooks
    attributes.rs           Attribute post-processor
    balances.rs             Balance post-processor

services/
  mod.rs                    ServicesBuilder — wires extractors, gateway, and server together
  rpc.rs                    HTTP endpoints: state snapshots, component queries
  ws.rs                     WebSocket broadcaster — emits BlockAggregatedChanges per block
  deltas_buffer.rs          PendingDeltasBuffer — pending-block state for RPC consistency
  cache.rs                  HTTP response cache
  api_docs.rs               OpenAPI schema generation (utoipa)
  access_control.rs         API-key authentication middleware
  client_metadata.rs        Parses bounded X-Tycho-Client-Metadata values for metric labels
  middleware/
    plan_restrictions.rs    Per-user API limits via X-User-Plan header (plans.yaml)
    compression.rs          Zstd response compression
    metrics.rs              RPC request metrics
    pagination.rs           Request pagination validation

substreams/                 gRPC client for Substreams streaming API (stream.rs)
pb/                         Auto-generated protobuf bindings (Substreams + Firehose)
```

## ProtocolExtractor

`ProtocolExtractor<G, T, E>` is generic over gateway `G`, token pre-processor `T`, and optional
extension `E`. It is the single point that turns raw Substreams messages into typed block data.

### Normal (full-block) path

1. Deserialize `BlockScopedData` → `BlockChanges` (tx-level state/balance deltas).
2. Run post-processor if configured.
3. Call `E::process_block_update()` (DCI — see below).
4. Fetch metadata for any new token addresses via `T` (ERC-20 symbol / decimals over RPC).
5. Insert `BlockChanges` into `ReorgBuffer`.
6. When `ReorgBuffer` has `>= commit_batch_size` blocks before `finalized_block_height`, drain them
   and schedule a DB write (see Persistence).
7. Aggregate all tx-level deltas → `BlockAggregatedChanges`; compute TVL.
8. Broadcast `Arc<BlockAggregatedChanges>` to subscribers.

### Partial-block path

Some chains (e.g. Base) emit multiple partial messages per block. While
`partial_block_index.is_some()`, each message is merged into `PartialBlockBuffer` (a single
`Option<BlockChanges>`). An aggregated message is broadcast immediately for WebSocket consumers,
but the block is **not** inserted into `ReorgBuffer` or written to DB until the full-block signal
arrives (clearing the buffer).

### DCI (Dynamic Contract Indexer) — optional

`ExtractorExtension::process_block_update()` is called on the accumulated `BlockChanges` before
aggregation. The DCI uses entry-point tracing results (`trace_results`) to extract additional
contract state and injects it back into `BlockChanges`. Two implementations exist:
`DynamicContractIndexer` (`dci.rs` — generic EVM tracing) and `HooksDCI` (`hooks/hook_dci.rs` —
UniswapV4-style hooks with permission detection and protocol-specific metadata via `integrations/`).
When no DCI is configured the call is a no-op.

### Revert handling

On `BlockUndoSignal(target_hash)` from Substreams:

1. `ReorgBuffer::purge(target_hash)` removes all buffered blocks after the common ancestor.
2. A `BlockAggregatedChanges` with `revert = true` is broadcast.
3. **No DB rollback is needed** — only finalized blocks ever reach the DB, so the persisted state
   is always on the canonical chain.

`PendingDeltasBuffer` (RPC side) mirrors this: it uses its own `ReorgBuffer` and discards
non-canonical pending blocks on the same broadcast.

## Persistence

`CachedGateway` enqueues `WriteOp` messages; `DBCacheWriteExecutor` flushes them when the next
block batch arrives. Writes follow a fixed FK-safe order (block → tx → contracts → tokens →
components → state → entry points → cursor). Every mutable row is versioned with `valid_from` /
`valid_to`; inserting a version closes the previous row's validity window (see
`tycho-storage/CLAUDE.md`).

**Trigger:** `ReorgBuffer::drain_blocks_until(finalized_height)` — blocks are only committed once
they are provably behind the finality horizon.

## Why extractor messages must be broadcast to the RPC service

HTTP snapshot endpoints must reflect the latest state, including blocks not yet committed to DB.
Every `BlockAggregatedChanges` message is sent to `PendingDeltasBuffer` (in addition to WebSocket
clients). When an RPC query arrives, the handler fetches the DB snapshot then applies in-memory
pending deltas on top, giving a consistent view up to the chain tip. Without this feed the RPC
would lag by however many blocks remain in `ReorgBuffer` awaiting finalization.

`db_committed_block_height` on each message tells `PendingDeltasBuffer` when a block has been
written; it auto-drains those blocks so memory usage stays bounded.

HTTP and WebSocket services parse `X-Tycho-Client-Metadata`, discard invalid or excess entries,
and expose an allowlisted subset as request/connection metric labels.

## Connections

```
ExtractorRunner (runner.rs)
  ├─ SubstreamsStream (substreams/) → ProtocolExtractor (protocol_extractor.rs)
  │    ├─ ReorgBuffer (reorg_buffer.rs) → CachedGateway → DB
  │    ├─ ProtocolMemoryCache (protocol_cache.rs)
  │    └─ DCIPlugin (dynamic_contract_indexer/) [optional]
  └─ broadcast Arc<BlockAggregatedChanges>
       ├─ WsService (services/ws.rs) → WebSocket clients
       └─ PendingDeltasBuffer (services/deltas_buffer.rs)
            └─ RpcHandlers (services/rpc.rs) → HTTP responses
```

## Client Sync

Changes to RPC endpoints in `services/rpc.rs` require updates to both `tycho-client/` (Rust) and
`tycho-client-py/` (Python). See `.claude/knowledge/python.md` for the Python sync checklist.

## RPC Endpoints

All POST under `/{version_prefix}/` (default `/v1/`), except where noted.

| Path | Description |
|---|---|
| `/contract_state` | Contract storage, code, balance |
| `/protocol_state` | Protocol component state + balances |
| `/tokens` | Token metadata (quality, decimals, symbol) |
| `/protocol_components` | Components by system / TVL / IDs |
| `/traced_entry_points` | Traced entry point results |
| `/add_entry_points` | Register entry points for tracing |
| `/protocol_systems` | List available protocol systems |
| `/component_tvl` | Component TVL estimates |
| `/health` | GET — health check |
| `/ws/` | GET — WebSocket upgrade for delta subscriptions |
