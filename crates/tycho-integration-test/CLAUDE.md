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

```bash
cargo run -p tycho-integration-test -- \
  --chain ethereum \
  --tycho-url ws://localhost:4242 \
  --tvl-threshold 100
```

Key optional flags: `--disable-onchain`, `--disable-rfq`, `--disable-execution`,
`--protocols uniswap_v2,curve`, `--max-blocks 100`, `--parallel-simulations 5`,
`--always-test-components <id,...>`.

## Module Structure

- **`main.rs`**: CLI (`Cli` struct), top-level orchestration loop — subscribes to Tycho,
  dispatches blocks to stream processors, calls `poll_rpc_for_block` for on-chain comparison
- **`stream_processor/`**:
  - `protocol_stream_processor.rs`: Handles on-chain protocol updates — applies deltas to
    `ProtocolSim` instances, runs `get_amount_out` simulations, validates via RPC execution
  - `rfq_stream_processor.rs`: Handles RFQ protocol updates — fetches live quotes, compares
    against simulation
- **`statistics.rs`**: `TestStatistics` + `ProtocolStatistics` — per-protocol counters for
  simulation success/failure, execution reverts, slippage, `get_limits` / `get_amount_out` calls
- **`metrics.rs`**: Prometheus metrics (served on `--metrics-port`, default 9898)

## What it validates

For each block update, for a random sample of components (up to `--max-simulations`):
1. `ProtocolSim::get_amount_out` — checks the simulation returns a non-zero output
2. `ProtocolSim::get_limits` — checks token limits are non-zero
3. Encodes a swap via `tycho-execution` and simulates it via RPC (`eth_call`) — checks it doesn't
   revert and that on-chain output is within slippage tolerance of simulated output
