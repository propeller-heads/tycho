# Token metadata deadlines and recovery

Issue #1317: one slow metadata RPC could hold the extractor's block publication open,
even with concurrent token processing. The deadline now covers the entire enrichment
collection, including queued tokens, metadata, transfer analysis, and RPC retries.

```text
Block discovers tokens
  -> enrich up to four tokens concurrently
  -> deadline reached: keep completed results, cancel unfinished requests
  -> persist unresolved identities as Pending through normal finalization
  -> publish the block; existing pools continue updating

Recovery worker reads finalized Pending rows
  -> retry metadata and transfer analysis
  -> commit complete metadata, then refresh caches
  -> client retries the affected pool's snapshot
  -> attach ready token metadata and apply intervening deltas
  -> decoder admits the pool if its quality meets the configured threshold
```

## Behavior and tradeoffs

- `Pending` is separate from quality. Its numeric fields are placeholders and must not
  be used for simulation. The decoder enforces readiness even at minimum quality zero.
- Only an RPC that does not answer produces `Pending`. A `symbol`/`decimals` call that
  reverts or returns undecodable data is a property of the token: it receives the same
  address/18-decimals fallbacks as before, and analysis decides the quality. A token with
  no funded owner gets the usual Bad verdict (quality 10) on both the hot path and in
  recovery, so a token deferred by a slow RPC ends up in the same state as one fetched
  on a fast RPC.
- Completed analysis retains the existing quality rules: good 100, fee token 50,
  bad 10. Infrastructure failures do not become an optimistic quality-100 verdict.
- New pools can be unavailable until finality, persistence, worker retries and snapshot
  acquisition finish. Tokens whose RPC keeps failing remain pending; their backlog is
  observable. Existing pools keep receiving blocks.
- One worker runs per indexer process, on the first configured chain, like the existing
  token pre-processor. It uses pages of 32, four concurrent attempts, a ten-second attempt
  timeout, and five seconds between pages; with nothing pending it checks once a minute.
  An ID cursor avoids starving later rows when earlier attempts fail or completed rows
  leave the queue. Each token that fails waits twice as long before its next attempt,
  from five seconds up to one hour, so a persistently failing token bounds its RPC cost
  without ever being given up on. The backoff is in memory and resets on restart; the
  worker resumes from durable rows.
- Repair writes only affect pending rows. Old creation inserts cannot overwrite repairs.
  Cache reconciliation also picks up repairs from another process or a lost commit response.
- Client retries run in the existing background snapshot path. A parked pool is re-checked
  after five seconds, then with the wait doubling up to five minutes, so a pool the server
  never repairs costs a bounded number of requests. Removals, explicit deletions and
  reverted creations cancel staged work; reorgs cancel old snapshots and refetch surviving
  pools. Partial blocks retain the existing completed-block snapshot boundary and delta
  catch-up.
- Metadata still uses `latest`; hot-path analysis uses the incoming block and recovery
  analysis uses `latest`. This does not introduce historical token-metadata versioning.

This bounds token enrichment, **not the whole block pipeline**. Database writes, DCI,
gRPC/WebSocket outages and runtime scheduling can still delay publication. A universal
“every block under three seconds” guarantee has not been established.

## Rollout

The default `TOKEN_ENRICHMENT_BUDGET_MS=0` preserves legacy enrichment behavior. Deferral
must be enabled explicitly; setting a deadline before upgrading consumers is unsafe.

1. Apply `2026-09-07_token_metadata_readiness` before starting the updated storage code.
   Existing rows default to ready; this does not repair previously stored fallback values.
2. Upgrade Rust clients/simulation and the Python client bundle. Custom consumers must check
   `metadata_status` and support refreshed `snapshots.tokens` before server deferral is enabled.
3. Deploy the indexer with the budget disabled, then enable a measured canary budget. For
   example, `TOKEN_ENRICHMENT_BUDGET_MS=1000` provides a one-second enrichment budget;
   one second is a provisional setting, not a production sizing result.
4. Measure unknown-token counts, enrichment latency, pending backlog/age and publication age.
   Standalone RPC processes refresh their token cache periodically, so they can add delay
   before clients observe a repair.

Setting the budget back to zero stops creating deadline-related pending rows. The recovery
worker continues processing existing rows. Keep the readiness-aware binaries and schema until
the pending queue is drained; dropping the column would discard unresolved recovery state.

## Observability

| Metric | Meaning |
| --- | --- |
| `token_enrichment_unknown_tokens` | Unique unknown tokens per enrichment invocation. |
| `token_enrichment_seconds` | Time spent collecting enrichment results. |
| `token_metadata_deferred` | Newly fetched tokens returned pending, excluding previously cached pending tokens. |
| `token_metadata_pending_count` | Finalized pending rows, sampled at the start of each worker sweep. |
| `token_metadata_pending_age_seconds` | Age of the oldest finalized pending row at that sample. |
| `token_metadata_recovery_attempts` | Recovery outcomes by `outcome`: `completed` (row repaired by this process), `already_complete` (repaired elsewhere first), `deferred` (RPC did not answer, retried with backoff), `commit_failed` (database write failed, retried). |
| `extractor_last_published_block_timestamp_seconds` | Block timestamp after extractor message fan-out completes. |

For Robinhood publication age, use
`time() - extractor_last_published_block_timestamp_seconds{chain="robinhood"}`.
Combine this with backlog age and process health; a heartbeat alone does not establish freshness.
These metrics do not install a production alert or dashboard. Endpoint round-trip latency and
production `new_tokens_count` distributions still need to be measured.

## Reading order

1. `protocol_extractor.rs`: `construct_currency_tokens`, called from `handle_tick_scoped_data`.
2. `token_pre_processor.rs`: `get_tokens`, then `fetch_token` and `call_metadata`.
3. `models/token.rs`: readiness contract; storage migration and `complete_token_metadata`.
4. `token_metadata_recovery.rs`: retries, commit and cache reconciliation.
5. Client `synchronizer.rs`: `fetch_snapshot`, `WaitingForTokens`, retry and delta catch-up.
6. Simulation `evm/decoder.rs`: readiness check before token admission and snapshot decoding.

Validation uses deterministic local RPC stalls, focused PostgreSQL transaction tests in an
isolated schema, cache tests, client lifecycle tests and decoder tests. The SQL fixture tests
the new migration and recovery queries; it does not reproduce every production extension or
the complete indexer deployment. Production incident replay and a full deployed end-to-end
test remain rollout work. The public-RPC benchmark numbers in the pull request measure the
concurrency and batching change on one endpoint; they do not validate the deadline or the
production workload.
