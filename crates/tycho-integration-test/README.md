# Tycho Integration Test

A comprehensive integration test that validates the Tycho system by performing end-to-end testing.
This test runs continuously in the cluster but it can also be run locally for debugging.

1. **Stream Processing**: Connects to Tycho indexer and receives live protocol state updates
2. **State Validation**: For each protocol update (after the first one) and for each swap direction:
    1. **Get Limits**: Retrieves maximum input/output limits for token pairs
    2. **Simulate Swap**: Calls `get_amount_out` with 0.1% of max input amount
    3. **Encode Transaction**: Builds the actual swap transaction via TychoRouterV3
    4. **Execute Simulation**: Runs `debug_traceCall` with necessary state overrides
    5. **Calculate Slippage**: Compares simulated vs actual execution results

## Supported Protocols

- **On-chain Protocols**: All DEX protocols supported by Tycho (Uniswap V2/V3, Curve, Balancer, etc.)
- **RFQ Protocols**: Request-for-Quote protocols like Hashflow
- **Price Level Stream**: Titan pAMM price level stream venues (`pricelevelstream:*`, Ethereum only)

## Configuration

### Environment Variables

| Variable        | Description                | Example                                    |
|-----------------|----------------------------|--------------------------------------------|
| `RUST_LOG`      | Logging configuration      | `tycho_integration_test=info,error`        |
| `TYCHO_URL`     | Tycho indexer endpoint     | `tycho-dev.propellerheads.xyz`             |
| `RPC_URL`       | Blockchain RPC endpoint    | `https://eth-mainnet.alchemyapi.io/v2/...` |
| `TYCHO_API_KEY` | API key for Tycho services | `your-api-key`                             |

### Command Line Arguments

| Argument                   | Default    | Description                                                                                                      |
|----------------------------|------------|------------------------------------------------------------------------------------------------------------------|
| `--chain`                  | `ethereum` | Blockchain to test against                                                                                       |
| `--protocols`              | -          | Comma-separated list of protocols to test. Defaults to all protocols                                             |
| `--max-simulations`        | `10`       | Maximum simulations (of updated states) per update                                                               |
| `--max-simulations-stale`  | `10`       | Maximum simulations (of stale states) per update per protocol                                                    |
| `--parallel-simulations`   | `5`        | Number of concurrent simulations                                                                                 |
| `--parallel-updates`       | `5`        | Number of concurrent update processors                                                                           |
| `--tvl-threshold`          | `100.0`    | TVL threshold in native tokens for filtering                                                                     |
| `--metrics-port`           | `9898`     | Port for Prometheus metrics server                                                                               |
| `--disable-onchain`        | `false`    | Skip on-chain protocol testing                                                                                   |
| `--disable-rfq`            | `false`    | Skip RFQ protocol testing                                                                                        |
| `--disable-price-level-stream` | `false` | Skip Titan pAMM price level stream testing (only active on Ethereum)                                            |
| `--price-level-stream-block-interval` | `1` | Emit one sampled price level update per this many blocks                                                    |
| `--price-level-stream-stale-threshold-secs` | `10` | Seconds without a Titan message before marking served pAMMs stale in metrics (0 disables)              |
| `--skip-messages-duration` | `600`      | RFQ stream skip duration (seconds) after processing a message                                                    |
| `--block-wait-time`        | `12`       | Time to wait (seconds) for block N+1 before executing debug_traceCall                                            |
| `--always-test-components` | -          | Comma-separated list of component IDs to test every block                                                        |
| `--max-blocks`             | `0`        | Maximum number of blocks to process before exiting (0 = run indefinitely). When set, prints a summary at the end |
| `--force-catch-up-every-n-blocks` | -   | Run the [catch-up check](#catch-up-check), forcing a resubscription about every this many blocks. Off when not set |

## Running Locally

```bash
# Set required environment variables
export RUST_LOG=tycho_integration_test=info,tycho_test=debug,error
export TYCHO_URL=tycho-dev.propellerheads.xyz
export TYCHO_API_KEY=<your-tycho-key>
export RPC_URL=https://your-rpc-endpoint

# Run with default settings
cargo run --package tycho-integration-test

# Run with custom parameters
cargo run --package tycho-integration-test -- \
  --chain ethereum \
  --disable-rfq \
  --protocols uniswap_v2 \
  --max-simulations 20 \
  --parallel-simulations 10

# Run with specific components to always test
cargo run --package tycho-integration-test -- \
  --always-test-components component_id_1,component_id_2,component_id_3

# Run for a specific number of blocks and get a summary
cargo run --package tycho-integration-test -- \
  --chain ethereum \
  --max-blocks 10 \
  --protocols uniswap_v2,uniswap_v3
```

## Catch-up check

`--force-catch-up-every-n-blocks N` checks `tycho-client`'s delta catch-up against the live server.
It runs a second stream of the same protocols, with the same TVL filter, next to the tests. About
every `N` blocks, each of its subscriptions drops its next 3 messages and ends, so the client
resubscribes having missed those blocks. A server with the delta catch-up endpoint lets the client
apply the missed blocks without a snapshot; otherwise the client falls back to a snapshot.

When the client has recovered, the check compares the state it holds for each protocol with a
snapshot of the same components at the same block: component attributes and balances, and the
storage, native balance, code and token balances of their contracts. Every resubscription of the
stream is checked, forced or not. TVL, tokens and DCI entry points are not compared.

The stream is wired like `TychoStreamBuilder` builds it, with a `DeltasClient` wrapper that ends the
subscriptions; the synchronizers and the catch-up itself are unchanged `tycho-client` code. It adds
one subscription per protocol, and a snapshot per protocol after each resubscription, to the
server's load.

```bash
cargo run --package tycho-integration-test -- \
  --chain ethereum \
  --protocols uniswap_v2,uniswap_v3 \
  --disable-rfq \
  --force-catch-up-every-n-blocks 300
```

### Reading the results

| Metric | Labels | Counts |
|--------|--------|--------|
| `tycho_integration_catch_up_resyncs_total` | `protocol`, `outcome` | How the client recovered from each resubscription: `caught_up` (applied the missed blocks without a snapshot), `snapshot` (fell back to a snapshot), `resumed` (missed no block), `nothing_tracked` (tracks no component, so the two look the same), or `no_resubscription` (did not resubscribe within 5 minutes of a forced gap) |
| `tycho_integration_catch_up_comparisons_total` | `protocol`, `resync`, `result` | Comparisons after a `caught_up` or `snapshot` recovery: `equal`, `known_gaps`, `mismatch`, or `error` (the snapshot could not be fetched) |
| `tycho_integration_catch_up_differences_total` | `protocol`, `field`, `status` | Entries that differ from the snapshot, by field and status |

- **The check proves nothing unless `outcome="caught_up"` grows.** If only `outcome="snapshot"`
  grows, the client never catches up: the server or its gateway does not serve the catch-up endpoint,
  or the server cannot serve the gap. `tycho-client` logs the reason at info or warn
  (`Delta catch-up not possible` / `Delta catch-up failed`, target
  `tycho_client::feed::synchronizer`), and the server counts `delta_catch_up_responses` by
  extractor and outcome.
- **`result="mismatch"` with `resync="caught_up"`** means the state after a catch-up differs from the
  snapshot in a field reverts restore (`status="mismatch"`). The check logs it at error, with up to
  10 of the differing entries.
- **`status="known_gap"`** differences are in native balances, code, account token balances, and
  contracts that only the DCI traces of tracked components reach. Reverts don't restore these yet,
  so they are reported but are not mismatches; the result is then `known_gaps`.
- Comparisons after a `snapshot` recovery are a control and should be `equal`.
- A difference after a catch-up is not necessarily caused by it: the state also carries everything
  the stream delivered before the gap.

To see the check's own logs and the client's catch-up decisions, include
`tycho_integration_test=info,tycho_client::feed::synchronizer=info` in `RUST_LOG`.
