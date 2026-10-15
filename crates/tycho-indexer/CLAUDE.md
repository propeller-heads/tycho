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
  supervisor.rs             ExtractorSupervisor: restart lifecycle with exponential backoff; owns the subscription map; counts rebuilds in `extractor_restarts_total` (registered at zero)
  factory.rs                ExtractorFactory: builds a fresh extractor + runner per (re)start; extractor config types; extractor fingerprint
  delta_format.rs           FormatStream (the committed delta format files, built in) and delta_format_digest
  reorg_buffer.rs           ReorgBuffer — finality-aware block queue; chain-reorg purge
  models.rs                 Re-exports the block types (defined in tycho-common's models/blockchain.rs); merge helpers + test fixtures
  protocol_cache.rs         ProtocolMemoryCache — in-process token/component metadata cache
  chain_state.rs            ChainState — tracks current tip and finality horizon
  u256_num.rs               U256 numeric utilities
  token_analysis_cron.rs    Background job: token quality / tax analysis
  dynamic_contract_indexer/ DCI optional extension (see below)
    dci.rs                  Core DCI: DynamicContractIndexer implementation
    cache.rs                DCI component/contract cache
    hooks/                  UniswapV4-style hooks DCI variant
      hook_dci.rs           HooksDCI — hooks-specific ExtractorExtension
      cache.rs              HooksDCICache — per-component processing state, layered by block
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
  off_worker.rs             OffWorker — builds and serializes large state responses on the blocking pool, bounded per endpoint and step
  ws.rs                     WebSocket broadcaster — emits BlockAggregatedChanges per block
  deltas_buffer.rs          PendingDeltas — facade over one DeltaWindow per extractor
  state/
    window.rs               DeltaWindow — fixed-depth block window; retention, fold-on-eviction; catch_up_path
    revert_journal.rs       RevertJournal — the revert messages a DeltaWindow applied, with the hashes they orphaned
    cache.rs                EntityCache — long-lived timestamped entity store (a FoldSink); EntityCache::load builds it from one StateSnapshotGateway read at startup
    service.rs              StateService — answers contract/protocol state from cache ⊕ window, or names a FallbackReason; EntityCacheSetup (the routing itself is in rpc.rs)
    shadow.rs               Shadow mode — samples state requests, runs the cache path next to the DB path, compares the normalized answers, records metrics
  cache.rs                  HTTP response cache
  api_docs.rs               OpenAPI schema generation (utoipa)
  access_control.rs         API-key authentication middleware
  client_metadata.rs        Parses the X-Tycho-Client-Metadata header into allowlisted Prometheus labels
  debug.rs                  heap_profile handler behind the `jemalloc` feature
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

1. Deserialize `BlockScopedData` → `BlockChanges` (tx-level state/balance deltas) via
   `tycho-protobuf`'s `TryFromMessage` conversions; its `DecodeError` converts into
   `ExtractionError`.
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

On `BlockUndoSignal(target_hash, target_number)` from Substreams:

1. `ReorgBuffer::purge_to(target_hash, target_number)` removes invalidated blocks. A hash
   match purges strictly after the target; when the hash is absent, a buffered target height
   purges from that height inclusive (stale copy). A hash miss is fatal when the
   target is below the buffer, at the buffer's oldest block (no predecessor left to
   anchor the revert), or above the buffer without a pending partial at exactly that height
   (the flashblocks case — the only legitimate target-ahead shape). Nonfatal hash-miss
   fallbacks log a warning and increment `extractor_revert_hash_miss`.
2. Pending partials are dropped only when above the target height; partials at the target
   height are the still-valid prefix of the last valid block and are kept.
3. Previous attribute values are restored from the buffer, then the DB. The buffer keeps
   drained blocks in a committing section until `CachedGateway` reports their write
   flushed, so lookups cover every block whose write has not landed and a DB miss proves
   that no prior state exists — the revert never waits on the commit task. Attributes
   first created inside the reverted range revert as deletions, also when the range
   deleted them again: a client that applied the creation and missed the deletion still
   holds them. A delta that holds an attribute as both
   created and deleted is malformed module output, so it goes through the lookup like a
   pre-existing attribute. Attributes with no prior value anywhere revert as deletions,
   emit one summary warning, and increment `extractor_revert_attr_miss` per attribute; its
   `component_known` label says whether the component is known anywhere: a
   `TxWithChanges.protocol_components` entry in buffer history or a row in the
   `protocol_component` table. `false` means an upstream module emitted state for a
   component Tycho never saw created. The extractor registers both label sets at zero at
   startup so the first miss is visible to `increase()`. Any hit means an upstream module
   emitted an Update or Deletion for an attribute that never had a Creation.
4. If nothing was invalidated, only the cursor advances — no message is emitted. Otherwise
   a `BlockAggregatedChanges` with `revert = true` is broadcast.
5. **No DB rollback is needed** — only finalized blocks ever reach the DB, so the persisted
   state is always on the canonical chain.

`PendingDeltas` (RPC side) mirrors this through its per-extractor `DeltaWindow`, using the
strict hash-only `purge` on the block named by the broadcast revert message. A revert to a block
below `min(finalized, db_committed)`, a revert to an unknown hash, or a block that does not
extend the window's chain is an error that ends the pump and the process: only the extractor's
replay can refill the window. On `ExtractorRestarted` the pump folds the window's committed
blocks into the sink and clears it; the restarted extractor replays everything above its
database cursor.

Each window journals the revert messages it applies, with the hashes of the blocks they
orphaned (`services/state/revert_journal.rs`). An entry stays while its target is no lower than
the parent of the window's oldest block, and a clear drops them all. `DeltaWindow::catch_up_path`
uses both to list the messages that take a client from its last block to a block the window holds:
the reverts that orphaned the client's block, then the canonical blocks. It follows
`catch_up_spec` in `tycho-common`'s `testing::delta_history`, checked by a property test in
`services/deltas_buffer.rs`.

## Persistence

`CachedGateway` enqueues `WriteOp` messages; `DBCacheWriteExecutor` flushes them when the next
block batch arrives. Writes follow a fixed FK-safe order (block → tx → contracts → tokens →
components → state → entry points → cursor). Every mutable row is versioned with `valid_from` /
`valid_to` — historical rows are never mutated (see `tycho-storage/CLAUDE.md`).

**Trigger:** `ReorgBuffer::drain_blocks_until(finalized_height)` — blocks are only committed once
they are provably behind the finality horizon.

Drained blocks stay in the buffer's committing section (shared with the commit task via
`Arc`) until `CachedGateway::flushed_block_height` passes them, so revert lookups can
still see them while their write is in flight.

## Why extractor messages must be broadcast to the RPC service

HTTP snapshot endpoints must reflect the latest state, including blocks not yet committed to DB.
Every `BlockAggregatedChanges` message is sent to `PendingDeltasBuffer` (in addition to WebSocket
clients). When an RPC query arrives, the handler fetches the DB snapshot then applies in-memory
pending deltas on top, giving a consistent view up to the chain tip. Without this feed the RPC
would lag by however many blocks remain in `ReorgBuffer` awaiting finalization.

`db_committed_block_height` on each message is the height whose writes reached Postgres
(`CachedGateway::flushed_block_height`), not the last drained block: during catch-up the gateway
holds the tail of a drained batch in memory until the next flush. It is one of the three
watermarks bounding retention.
Each extractor's `DeltaWindow` (`services/state/window.rs`) keeps a block until it is at or below
`min(finalized, db_committed, tip - depth)`, then folds it into a `FoldSink` and evicts it.
Committed blocks are therefore retained and served, so window contents and DB rows overlap by up
to `depth` blocks: readers that merge both sides must bound window reads by `db_committed + 1`.
Depth and fold batching come from `--delta-window-depth` (default 128) and
`--delta-window-fold-batch` (default 1).
Metrics, labelled by `extractor`: `delta_window_fold_duration_ms` (summary, one sample per
folded block), `delta_window_blocks` and `delta_window_tip_block` (gauges, set after every insert
and every restart clear; an empty window reports 0 blocks and keeps its last tip).

With `--entity-cache-mode shadow|serve`, `main.rs` builds the `EntityCache` from one database
snapshot after the extractors are built and before the server starts (`EntityCache::load` in
`services/state/cache.rs`), and hands it to the services. The windows fold into it, under the
window lock. A failed load is a setup error and ends the process. `off` (the default) skips the
load, and the windows fold into `DiscardSink`.

In `serve`, `RpcHandler` (`services/rpc.rs`) asks the `StateService` (`services/state/service.rs`)
first for `/contract_state` and `/protocol_state`. The service answers from the cache plus the
window changes up to the requested version, without reading the database. A request it cannot
answer comes back as a `FallbackReason`, and the handler answers it from the database path and
counts it in `db_path_requests{endpoint, reason}`. `shadow` answers every request from the
database path.

In `shadow`, a sampled `/contract_state` or `/protocol_state` request runs the database path (the
client's answer), then `StateService`, and `services/state/shadow.rs` compares the two.
It erases the known differences from both answers and requires the rest to be equal. On a
mismatch it logs one warning with up to 20 diffs (`entity.field.key: db=… cache=…`).
A `/contract_state` comparison where every differing account has an unsaved change in another
extractor's window, at or below the version, counts as `known_mismatch` and logs its diffs at
`debug`, not as a warning: the cache path applies that change, the database path does not.
A comparison is discarded when it straddles a change: the window generation or the cache fold
count moved during it.
`--entity-cache-shadow-sample-rate` (default 0.0) sets the share. Metrics:
`entity_cache_shadow_comparisons_total{endpoint,outcome}`,
`entity_cache_shadow_duration_seconds{endpoint}`, `entity_cache_shadow_sample_rate`.

## Delta format files

`extractor/delta_format/*.json` hold every message the recorded extractors emit for their
recorded streams, revert messages included (`extractor/recorded_extractors/`: real extractors on
Postgres, substreams streams written with the protobuf builders, RPC services mocked):

- `native.json`: the native stream.
- `native_partial_blocks.json`: the same blocks sent as partial blocks, through the runner's
  dispatch with partial blocks enabled: a first block sent whole with its partial copy, partial
  messages, the full-block messages merged from them, reverts that drop pending partials, and an
  undo signal naming the block being streamed. Its full-block messages, reverts included, equal
  `native.json`'s for the same blocks, in the same order, and the `native.json` messages it skips
  are blocks a later revert orphans and reverts whose orphaned blocks were all skipped
  (`partial_delivery_emits_the_full_block_messages_of_full_delivery`). This is why the
  `partial_blocks` flag is not in the extractor fingerprint.
- `dci.json`: the DCI stream, including an entry point the mocked tracer never traces (the
  `paused` attribute, its retries and the retry limit).

The JSON is canonical: watermarks reset, object keys and sets sorted, and block `ts` and
component `created_at` written as their offset from the stream's base timestamp after the Unix
epoch, so the files do not change from day to day. `.gitattributes` checks them out with LF line
endings.

`test_serial_db_emitted_deltas_match_the_committed_format` (`extractor/delta_format_tests.rs`)
fails while the emitted messages differ from the files. A diff in them is a change to the deltas
clients receive for the same blocks. When the change is intended, regenerate the files against a
test database (`DATABASE_URL`) and commit them with it:

```sh
UPDATE_DELTA_FORMAT=1 cargo nextest run -p tycho-indexer --all-features --test-threads 1 \
  -E 'test(test_serial_db_emitted_deltas_match_the_committed_format)'
```

The binary embeds the files (`FormatStream` in `extractor/delta_format.rs`), and the tests compare
against the embedded bytes. A new stream needs a `FormatStream` variant and its file, created
empty before the first `UPDATE_DELTA_FORMAT=1` run; `every_format_file_belongs_to_a_stream` fails
while a file in the folder belongs to no stream in `FormatStream::ALL`.

`the_committed_format_exercises_every_field` fails while a field or enum variant of the message
or its nested types is empty, default or absent in every committed message; exercise it in a
recorded stream, or list it in `UNEXERCISED` with the reason.

The files cannot show behaviour behind the mocked RPC services, extractor modules the recorded
extractors do not run (the Uniswap V4 hooks DCI, the post-processors), or the revert of a block
sealed under a hash substreams no longer names (`HeightMatch` in `handle_revert`, which a
consistent stream never sends; covered by `test_revert_hash_miss_purges_from_target_height`).

## Extractor fingerprint

`ExtractorFactory::create` reads the extractor's substreams package once, downloading it from S3
when it is missing, and computes the extractor's `ExtractorFingerprint` from it
(`extractor_fingerprint` in `factory.rs`): Keccak-256 over length-prefixed fields holding

- `delta_format_digest()` (`extractor/delta_format.rs`): the committed delta format files,
  `tycho_ethereum::SERVICES_SOURCE_DIGEST` (the RPC services the recorded extractors mock), and
  `UNRECORDED_MODULES_SOURCE_DIGEST`, which `build.rs` computes (with tycho-ethereum's
  `build/source_digest.rs`) from every file under
  `extractor/dynamic_contract_indexer/hooks/` and `extractor/post_processors/` (modules the
  recorded extractors do not run) and from the code only they use, `extractor/u256_num.rs` and
  `extractor/dynamic_contract_indexer/mod.rs`;
- the package's bytes, not its path;
- the config fields that shape deltas: `name`, `chain`, `start_block`, `module_name`,
  `initialized_accounts`, `initialized_accounts_block`, `dci_plugin` (the pool manager as parsed
  address bytes; a value that is not hex fails startup) and `post_processor`. The function
  destructures `ExtractorConfig` without `..` and binds every other field to `_` with the reason,
  so a new field fails to compile until it is classified;
- the inputs outside the config that shape deltas (`FingerprintInputs`, also destructured without
  `..`): the token pre-processor's settlement contract (`--settlement-contract`, which sets new
  tokens' quality, tax and gas), the chain's native token address (built in, or `native` in
  `chains.yaml` for a custom chain; the hooks DCI's state overrides and the storage's account
  balances read it), and, for extractors with a DCI plugin, the DCI blacklist
  (`DCI_BLACKLIST_ADDRESSES`, as a set: it decides full versus traced-slot indexing).

Equal fingerprints mean two runs use the same code, package and config, so they emit the same
deltas for the same blocks as far as those decide them. What the runs read at startup or from
the network can still differ: the database contents (token metadata already stored, the DCI's
snapshot of ERC-20 addresses, and its trace retry counts, which are not persisted) and the RPC
node's answers. The fingerprint changes with the package, those config fields and inputs, the
format files or those sources; a plain restart of the same build with the same package and config
keeps it. The factory keeps the decoded
package, and every run it builds, in-process restarts included, streams that package, so an
extractor's fingerprint is fixed for the process's lifetime. `ExtractorSupervisor::handle` puts
it on the `ExtractorHandle` (`MessageSender::fingerprint`).

Not in the fingerprint:

- The `partial_blocks` flag: partial delivery yields the same full-block messages (see "Delta
  format files"), and partial-block subscribers always resync from a snapshot.
- The `TRACE_*` variables: they pick the tracing node and its retry policy, not what is traced
  (see `create_rpc_dci`).
- The code of `main.rs`'s `initialize_accounts`: its config fields are hashed, its code is not.
- The rest of a custom chain's config (`chains.yaml`): besides the native token, the indexer only
  reads its block time, which estimates the chain tip for sync-progress metrics.
- The delivery path after the extractor, which neither the format files nor the source digests
  cover: the model-to-DTO conversion (`dto::BlockAggregatedChanges::from`), `drop_state` for
  `include_state: false` subscriptions, and `ws.rs`'s filter of partial and full-block messages.

The source digests ignore what cannot change behaviour: in `.rs` files they hash the `syn` token
stream without comments, doc comments (`///`, `//!`, `#[doc]`), formatting, `#[cfg(test)]` items
and `#[test]`-style functions (see tycho-ethereum's `CLAUDE.md`). Any other edit to a hashed
source changes the fingerprint, so clients resync after the next deploy even when the deltas did
not change; so does a `syn`, `quote` or `proc-macro2` upgrade that changes the token stream's
string form, once.

## Connections

```
ExtractorSupervisor (supervisor.rs) — rebuilds the runner via ExtractorFactory (factory.rs) on failure
  └─ ExtractorRunner (runner.rs)
       ├─ SubstreamsStream (substreams/) → ProtocolExtractor (protocol_extractor.rs)
       │    ├─ ReorgBuffer (reorg_buffer.rs) → CachedGateway → DB
       │    ├─ ProtocolMemoryCache (protocol_cache.rs)
       │    └─ DCIPlugin (dynamic_contract_indexer/) [optional]
       └─ broadcast DeltaCommand (Block | ExtractorRestarted)
            ├─ WsService (services/ws.rs) → WebSocket clients
            └─ PendingDeltas (services/deltas_buffer.rs)
                 └─ DeltaWindow per extractor (services/state/window.rs)
                      ├─ folds into EntityCache (services/state/cache.rs) [shadow|serve], else DiscardSink
                      ├─ StateService (services/state/service.rs) ← EntityCache [serve]
                      └─ RpcHandlers (services/rpc.rs) → HTTP responses
                           (state endpoints: StateService first, database path on a fallback)
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
| `/ws` | GET — WebSocket upgrade for delta subscriptions (exact match; `/ws/` 404s) |

Registered outside the version prefix: `GET /docs/{...}` (Swagger UI, schema at
`/api-docs/openapi.json`) and `GET /debug/pprof/heap` (API-key gated, behind the `jemalloc`
feature).
