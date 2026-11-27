# tycho-storage

PostgreSQL backend implementing the storage traits defined in `tycho-common`.

## Old gateway: off

The gateway on the old tables is off. The modules `chain`, `contract`, `protocol`,
`component_pause`, `token_cache`, `component_index`, `entry_point`, `extraction_state`,
`versioning` and `direct`, the `PostgresGateway` type, the entity-cache reads in `snapshot`, and
their tests are marked `#[cfg(old_gateway)]`. That cfg is never set, so this code is not compiled.
The cut-over deletes it.

`CachedGateway` keeps the buffered write path and sends every batch to
`execute_instance_write_op`. These fail with `StorageError::Unsupported`: a batch without an
instance, every read, and every direct write that only the old gateway served.
`GatewayBuilder::enable_token_cache` and `set_component_index` still set their flags, but nothing
reads them until the token cache and the component index are ported. The module map
and the Architecture section below still describe the old gateway.

## Module Map

```
postgres/
├── mod.rs              — PostgresGateway (internal); shared enum caches; DB init helpers
├── pool.rs             — checkout helper (span, wait histogram) and `db_pool_*` status gauges
├── builder.rs          — GatewayBuilder: configures + constructs public gateways
├── cache.rs            — CachedGateway + DBCacheWriteExecutor: buffered write path
├── direct.rs           — DirectGateway: unbuffered read/write path (testing, auditing)
├── chain.rs            — block & transaction persistence
├── contract.rs         — account, code, storage slot, and native-balance persistence
├── protocol.rs         — protocol component, attribute, and token-balance persistence
├── component_pause.rs  — admin pause persistence (ComponentPauseGateway)
├── token_cache.rs      — in-memory token store answering get_tokens without SQL (opt-in)
├── component_index.rs  — in-memory id/TVL index counting and paging get_protocol_components
├── entry_point.rs      — entry point + tracing param/result persistence
├── extraction_state.rs — extractor checkpoint (cursor, block hash) persistence
├── snapshot.rs         — one-transaction read of all live contracts and components for the entity cache (StateSnapshotGateway)
├── versioning.rs       — VersionedRow / StoredVersionedRow + apply_versioning(); PartitionedVersionedRow + apply_partitioned_versioning()
├── orm.rs              — Diesel Queryable/Insertable structs for every table
├── schema.rs           — auto-generated Diesel table! macros
└── instance/           — per-instance `tycho` schema; written by gateways bound to an instance, not yet read by the public gateways; callers run each read in one `snapshot_transaction`
    ├── mod.rs          — `InstanceId` newtype; joins with the shared `public` tables
    ├── active.rs       — the active instance of one protocol and of all protocols, looked up once per read from `protocol_system.active_instance_id`
    ├── create.rs       — `ensure_instance`: find or create an active instance by label, called through `CachedGateway::ensure_instance`; for initialized accounts until extractors create theirs
    ├── gateway.rs      — PostgresGatewayNew, inner gateway on the instance tables (renamed to PostgresGateway at cut-over)
    ├── origins.rs      — BatchOrigins: block number, timestamp and tx index of each change in a batch
    ├── read/           — its read methods, one file per domain
    │   ├── component.rs — protocol components and their TVL from the active instance of each protocol
    │   ├── contract.rs — contracts, slots and account balances of a protocol's active instance at the latest version
    │   ├── state.rs    — protocol state and component balances of the active instance at the latest version
    │   ├── token.rs    — tokens, prices and token owners; tokens by `token.address`, owners and the traded filter from the active instances
    │   └── version.rs  — the check that the current tables can answer a version
    ├── write/          — its write methods, one file per domain
    │   ├── account_balance.rs — account balances (native and token): their key, history row and table writes for versioning
    │   ├── balance.rs  — component balances of a batch, and their key, history row and table writes for versioning
    │   ├── block.rs    — blocks, in the shared `public.block` table; the old gateway calls its SQL too
    │   ├── component.rs — components of a batch with their token and contract links (a re-sent component is skipped with its links; one with another creation tx is logged and counted); the component id of each external id, in one instance; the protocol type and protocol system checks
    │   ├── contract.rs — accounts of a batch (the first creation wins), account deltas and token balances; the account id of each address, in one instance
    │   ├── contract_code.rs — contract code rows: their key, history row and table writes for versioning; the same hash adds no version
    │   ├── contract_storage.rs — contract storage rows: their key, history row and table writes for versioning
    │   ├── extraction_state.rs — cursor and last block of each instance
    │   ├── state.rs    — state deltas of a batch, and protocol state rows: their key, history row and table writes for versioning
    │   ├── token.rs    — tokens in the shared `public.token` table, by address; the first writer of a token wins; the stored-token check of balance writes
    │   └── versioning.rs — VersionedRowNew (renamed to VersionedRow at cut-over) + write_versions(): latest version in the current table, replaced versions in history
    ├── orm.rs          — Diesel row and insert structs for every `tycho` table
    ├── schema.rs       — auto-generated (`[print_schema.instance]` in diesel.toml)
    └── tests.rs        — schema behaviour: leaves, FKs, partition routing, history days
```

## Instance schema (`tycho`)

The protocol-isolation schema lives next to the tables above until the data migration moves each
protocol into an instance. Every per-instance table is list-partitioned by `instance_id`, one leaf
per instance (`<table>_i<id>`), and every key starts with `instance_id`. History of the five
versioned entities is in `*_history` tables, partitioned by day on `valid_to` across all
instances. No table has a DEFAULT partition.

- Create an instance's leaves with `tycho.create_instance_leaves(id)` then
  `tycho.attach_instance_leaves(id)`; an import loads between the two. Foreign keys exist only
  between the leaves of one instance. Tests use `db_fixtures::insert_instance` on a connection from
  `testing::instance_test_connection()`, which runs instance tests one at a time.
- History days come from `tycho.premake_history_partitions()` (pg_cron, 00:15), not from
  pg_partman's `run_maintenance()`, which makes no day for a partition set without rows.
  `drop_expired_partitions()` drops expired days of both the old partitioned tables and the
  history tables.
- `public.protocol_system.active_instance_id` records which instance readers see.
  `public.token.address` is the key of a token (NOT NULL, UNIQUE; one chain per database), and
  instance tables refer to tokens by it. A token has no `account` row.
- `CachedGateway::for_instance(id)` makes a gateway whose batches the write executor runs through
  `execute_instance_write_op` on `PostgresGatewayNew`. No transactions are stored: versioned rows
  take their block timestamp and tx index from the batch (`instance::origins`), and `UpsertTx`
  writes nothing. It writes blocks, tokens, components, protocol state, component balances,
  accounts, contract code, storage, account balances and `tycho.extraction_state` so far; any
  other op fails the whole batch with `StorageError::Unsupported`. At cut-over, the old gateway
  is deleted and `PostgresGatewayNew` is renamed.
- `PostgresGatewayNew::write_versions` versions the rows of an entity that implements
  `instance::write::versioning::VersionedRowNew` (protocol state, component balances, contract
  code, contract storage and account balances). The module doc of `write/versioning.rs` has the
  rules, repeats included. A change that does not start after the version it replaces fails the
  batch. The old versioning (`postgres/versioning.rs`) compares a change only with the stored
  row. It skips a change that is not after that row. It does not compare the changes of one key
  inside a batch, so it applies two changes with the same start.

## Architecture

All public DB operations go through one of two gateway structs:

- **`CachedGateway`** (normal path): sends `WriteOp` messages over an async channel to
  `DBCacheWriteExecutor`, which batches by block and flushes in a fixed order when the next
  block arrives. Most reads hit the DB directly; the exceptions are `get_tokens` (served from
  `token_cache` when enabled), `get_protocol_components` (counted and paged by `component_index`
  when enabled) and `get_delta` (small LRU). The executor runs a batch up to
  three times when Postgres reports a transaction conflict (deadlock or serialization
  failure); each attempt re-runs from scratch with a fresh snapshot. Admin pause writes
  (`pause_components`, `unpause_components`) skip the executor and run on their own connection,
  because they are not block data.
- **`DirectGateway`** (testing / low-throughput): same trait surface, no buffering.

`GatewayBuilder::build` requires **exactly one** chain (`ensure_chain`) — an instance is
single-chain; it hard-fails otherwise.

Both delegate every actual SQL call to `PostgresGateway` (unexported). Domain logic lives in
`chain`, `contract`, `protocol`, `component_pause`, `entry_point`, `extraction_state`, and
`snapshot`—each adding methods to `PostgresGateway` via `impl` blocks in their own file.

`versioning` is the only module without a DB table of its own; it provides the shared traits
and utilities consumed by `contract` and `protocol`. Two paths: `apply_versioning()` for plain
versioned tables, and `apply_partitioned_versioning()` for the partitioned ones — protocol state,
component balances, and contract storage.

`token_cache` holds an in-memory copy of the token tables so `get_tokens` never touches SQL.
Opt-in via `GatewayBuilder::enable_token_cache()` (the `index` and `rpc` commands enable it;
the token-analysis job does not). Kept fresh by write-through on token/balance writes plus a
periodic `modified_ts` delta poll for out-of-process writers. See the module docs for design.

`component_index` holds the id and TVL of every component, per protocol system, so
`get_protocol_components` requests with a protocol system and no ids skip the SQL `COUNT` and
`OFFSET` scan; Postgres only loads the components on the page, by primary key. Enabled via
`GatewayBuilder::set_component_index` (the `--component-index` flag / `COMPONENT_INDEX` env var of
the `index` and `rpc` commands, on by default). Kept fresh by a 1 s poll of new component ids and
of `component_tvl` rows with a newer `modified_ts` (`idx_component_tvl_modified_ts`), and a full
reload every hour. See the module docs for design and for the unexpected cases only the full
reload corrects.

Every gateway checkout and the DB writer checkout go through `pool::checkout`. It opens a
`db_pool_checkout` span and records the wait in `db_pool_checkout_duration_seconds{pool}`. The
`pool` label is `extraction`, `rpc` or `direct`. The `rpc` and `direct` pools serve requests and set
`plan_cache_mode = force_custom_plan` on each connection: a generic plan locks every
instance-table partition. The `extraction` pool keeps `auto`. A 1 s sampler sets
`db_pool_connections{pool,state}`, `db_pool_waiting{pool}` and `db_pool_max_size{pool}`. Traced
entry point reads load rows with a connection (`load_traced_entry_point_rows`) and return the
connection before they decode the JSON (`decode_traced_entry_points`). Do not hold a connection
during a decode.

## Write Order

`DBCacheWriteExecutor` flushes ops in this fixed sequence to satisfy FK constraints:

1. `UpsertBlock` → `UpsertTx` → `InsertContract` → `UpdateContracts`
2. `InsertTokens` → `UpdateTokens` → `InsertAccountBalances`
3. `InsertProtocolComponents` → `InsertComponentBalances` → `UpsertProtocolState`
4. `InsertEntryPoints` → `InsertEntryPointTracingParams` → `UpsertTracedEntryPoints`
5. `SaveExtractionState`

## Temporal Model

Every mutable entity carries `valid_from` / `valid_to` timestamps enabling time-travel
queries. `versioning::apply_versioning()` sets `valid_to` on the previous row when a new
version is inserted. Historical rows are never mutated.

History is bounded, though: pg_cron jobs prune it outside the Rust code. `drop_expired_partitions()`
drops expired partitions of `protocol_state`, `component_balance` and `contract_storage` (and of
the `tycho.*_history` tables), and
`cleanup_orphaned_transactions()` deletes transaction rows nothing references any more (tracked via
`transaction_cleanup_progress`). Both read the horizon from the `partition_retention_config` table
(default 1 month). See `scripts/prune_transaction_table.md`.
