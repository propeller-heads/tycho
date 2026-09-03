# tycho-integration-test

Long-running binary (not a `cargo test` suite) that connects to a live Tycho instance, syncs
protocol state via `tycho-client`, and continuously validates simulation accuracy against on-chain
results. Exits after `--max-blocks` blocks, or runs indefinitely.

## Running

Requires three env vars (can be set in `.claude/settings.local.json`):

| Variable | Purpose |
|---|---|
| `TYCHO_URL` | WebSocket endpoint of the Tycho server |
| `TYCHO_API_KEY` | Auth key (`sampletoken` works against local dev instances) |
| `RPC_URL` | Ethereum-compatible JSON-RPC endpoint for on-chain validation |

Both the token loader and the protocol stream default to TLS (`https`/`wss`). Pass `--no-tls`
(or set `TYCHO_NO_TLS=true`) when pointing at a local dev instance served over plain HTTP.

```bash
cargo run -p tycho-integration-test -- \
  --chain ethereum \
  --tycho-url 127.0.0.1:4242 \
  --no-tls \
  --tvl-threshold 100
```

Key optional flags: `--no-tls`, `--disable-onchain`, `--disable-rfq`,
`--disable-price-level-stream`, `--disable-execution`, `--protocols uniswap_v2,curve`,
`--max-blocks 100`, `--parallel-simulations 5`, `--always-test-components <id,...>`,
`--price-level-stream-block-interval 1`, `--price-level-stream-stale-threshold-secs 10`,
`--test-every-n-blocks 10`, `--bypass-executor-timelock`.

`--bypass-executor-timelock` writes `executorsActivationTimestamp = 1` for every executor of the
chain into the execution simulation's state overrides, so an executor that is unapproved or still
inside its 3-day activation timelock does not make `Dispatcher._validateExecutor` revert. The
router keeps its deployed bytecode, and only the read-only simulation call is affected. It cannot
help an executor that has no bytecode deployed. Off by default so that a missing activation still
shows up as a failure.

## Module Structure

- **`main.rs`**: CLI (`Cli` struct), top-level orchestration loop — subscribes to Tycho,
  dispatches blocks to stream processors, calls `poll_rpc_for_block` (every-block mode, and
  sampled mode under `--partial-blocks`) or `await_target_block` (`--test-every-n-blocks` > 1
  without `--partial-blocks`, fetches the sampled block by number) for on-chain comparison
- **`stream_processor/`**:
  - `protocol_stream_processor.rs`: Handles on-chain protocol updates — applies deltas to
    `ProtocolSim` instances, runs `get_amount_out` simulations, validates via RPC execution
  - `rfq_stream_processor.rs`: Handles RFQ protocol updates — fetches live quotes, compares
    against simulation
  - `price_level_stream_processor.rs`: Handles Titan pAMM price level stream updates (Ethereum
    only) — emits one sampled update per `--price-level-stream-block-interval` blocks, holding a
    chosen block's latest snapshot back until the stream moves to the next block (least drift to
    the finalized block), samples pair states, validates `get_limits` / `get_amount_out`. Marks
    the served venues stale in metrics when no Titan message arrives within
    `--price-level-stream-stale-threshold-secs`. Execution is simulated at the quoted block with
    the overrides `oracle_overrides.rs` collected for it. Venues on the PropAMMRouter whitelist
    are served under `propammfallback:*` and execute through the router; the others stay on
    `pricelevelstream:*`. Both families resolve through their single `pricelevelstream` /
    `propammfallback` entry in `executor_addresses.json` (the generic PropAMMExecutor and the
    PropAMMFallbackExecutor). Without overrides for its venue a swap falls to the router's Uniswap
    V3 fallback, counted per venue by
    `tycho_integration_price_level_oracle_override_misses_total` (Titan published none for that
    block) or `tycho_integration_price_level_oracle_override_unserved_total` (Titan serves no
    channel for the venue); a venue called directly reverts `StaleUpdate`
    (`tycho_integration_execution_stale_quotes_total`)
- **`oracle_overrides.rs`**: `OracleOverrides` — keeps the storage overrides Titan's
  `pamm_quote_stream` publishes, per quoted block, for the venues Titan serves (`vm:fermiswap`,
  `vm:kipseli`, `vm:bopamm`). Each frame is a venue's whole override set, so a venue's newest
  frame replaces its previous one for that block; venues are merged only on read
- **`statistics.rs`**: `TestStatistics` + `ProtocolStatistics` — per-protocol counters for
  simulation success/failure, execution reverts, slippage, `get_limits` / `get_amount_out` calls
- **`metrics.rs`**: Prometheus metrics (served on `--metrics-port`, default 9898)

## What it validates

For each block update, for a random sample of components (up to `--max-simulations`):
1. `ProtocolSim::get_amount_out` — checks the simulation returns a non-zero output
2. `ProtocolSim::get_limits` — checks token limits are non-zero
3. Encodes a swap via `tycho-execution` and simulates it via RPC (`debug_traceCall` with balance
   and allowance overrides) — checks it doesn't revert and that on-chain output is within
   slippage tolerance of simulated output
