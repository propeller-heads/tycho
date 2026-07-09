# Dynamic Contract Indexer (DCI)

Per-extractor plugin that discovers and indexes contracts reachable from declared
entrypoints (e.g. rate oracles called by pools), beyond what the substreams package
extracts. Key files: `crates/tycho-indexer/src/extractor/dynamic_contract_indexer/dci.rs`,
`crates/tycho-ethereum/src/services/entrypoint_tracer/tracer.rs`,
`protocols/substreams/crates/tycho-substreams/src/block_storage.rs`.

## How it works

- **Input feed**: `BlockChanges.block_contract_changes`, emitted by the substreams
  package via `get_block_storage_changes` (extended block model required). Carries
  per-tx storage changes and native balance changes (`tycho-substreams` ≥ 0.6.0;
  packages pinning older versions feed no balance updates).
- **Entrypoints**: `target:signature` + tracing params, traced via `debug_traceCall`
  (prestate tracer). A trace yields `accessed_slots` (accounts → slots) and `retriggers`.
- **Tracked contracts** get continuous storage/balance updates from the input feed.
  Re-traces snapshot only newly discovered accounts/slots — existing ones are not
  refreshed.
- **Retriggers**: slots whose value contains a called address (proxy impl slots always
  qualify). When that address segment changes, the entrypoint re-traces in the same
  block. Edge-triggered: missed or failed events are not replayed.

## Failure semantics

- A failed trace pauses every component using the entrypoint (`paused = 0x02`,
  `PausingReason::TracingError`). Nothing unpauses automatically, and consumers do not
  uniformly honor the flag.
- Failed params are retried up to `max_retry_count` (5), only on blocks where an
  associated component has state/balance activity. Counters are in-memory; after a
  restart, a stored successful result is rehydrated from the DB and the entrypoint is
  treated as traced — no further retries.

## DB relations

`entry_point` (external_id = `target:signature`) → `entry_point_tracing_params` →
`entry_point_tracing_result` (unique per params; `detection_data` jsonb holds
`retriggers` + `accessed_slots`, merged across traces). Accounts served in client
snapshots: `entry_point_tracing_params_calls_account`. Component links:
`protocol_component_uses_entry_point`. A new component reusing an existing
entrypoint+params reuses the cached result — no fresh trace.

## Forcing a re-trace

Delete the `entry_point_tracing_result` row (keep `entry_point`, params, and link
tables), restart the extractor, then wait for activity on an associated component.
Afterwards verify `calls_account` and clear stale `paused` attributes manually.
