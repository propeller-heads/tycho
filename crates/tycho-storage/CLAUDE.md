# tycho-storage

PostgreSQL backend implementing the storage traits defined in `tycho-common`.

## Module Map

```
postgres/
├── mod.rs              — PostgresGateway (internal); shared enum caches; DB init helpers
├── builder.rs          — GatewayBuilder: configures + constructs public gateways
├── cache.rs            — CachedGateway + DBCacheWriteExecutor: buffered write path
├── direct.rs           — DirectGateway: unbuffered read/write path (testing, auditing)
├── chain.rs            — block & transaction persistence
├── contract.rs         — account, code, storage slot, and native-balance persistence
├── protocol.rs         — protocol component, attribute, and token-balance persistence
├── token_cache.rs      — in-memory token store answering get_tokens without SQL (opt-in)
├── entry_point.rs      — entry point + tracing param/result persistence
├── extraction_state.rs — extractor checkpoint (cursor, block hash) persistence
├── versioning.rs       — VersionedRow / StoredVersionedRow traits + apply_versioning()
├── orm.rs              — Diesel Queryable/Insertable structs for every table
└── schema.rs           — auto-generated Diesel table! macros
```

## Architecture

All public DB operations go through one of two gateway structs:

- **`CachedGateway`** (normal path): sends `WriteOp` messages over an async channel to
  `DBCacheWriteExecutor`, which batches by block and flushes in a fixed order when the next
  block arrives. Reads bypass the cache and hit the DB directly.
- **`DirectGateway`** (testing / low-throughput): same trait surface, no buffering.

Both delegate every actual SQL call to `PostgresGateway` (unexported). Domain logic lives in
`chain`, `contract`, `protocol`, `entry_point`, and `extraction_state`—each adding methods to
`PostgresGateway` via `impl` blocks in their own file.

`versioning` is the only module without a DB table of its own; it provides the shared traits
and `apply_versioning()` utility consumed by `contract` and `protocol`.

`token_cache` holds an in-memory copy of the token tables so `get_tokens` never touches SQL.
Opt-in via `GatewayBuilder::enable_token_cache()` (the `index` and `rpc` commands enable it;
the token-analysis job does not). Kept fresh by write-through on token/balance writes plus a
periodic `modified_ts` delta poll for out-of-process writers. See the module docs for design.

## Write Order

`DBCacheWriteExecutor` flushes ops in this fixed sequence to satisfy FK constraints:

1. `UpsertBlock` → `UpsertTx` → `InsertContract` → `UpdateContracts`
2. `InsertTokens` → `UpdateTokens` → `InsertAccountBalances`
3. `InsertProtocolComponents` → `InsertComponentBalances` → `UpsertProtocolState`
4. `InsertEntryPoints` → `InsertEntryPointTracingParams` → `UpsertTracedEntryPoints`
5. `SaveExtractionState`

## Temporal Model

Every mutable entity carries `valid_from` / `valid_to` timestamps enabling time-travel
queries. `versioning::apply_versioning()` updates the previous row's `valid_to` to close its
validity window, then inserts the new version; historical payloads are otherwise preserved.
