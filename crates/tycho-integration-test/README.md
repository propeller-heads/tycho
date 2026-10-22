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
| `--protocol-tvl-thresholds` | -         | Comma-separated `protocol=threshold` pairs. Each one replaces `--tvl-threshold` for its protocol. Every named protocol must be enabled |
| `--metrics-port`           | `9898`     | Port for Prometheus metrics server                                                                               |
| `--disable-onchain`        | `false`    | Skip on-chain protocol testing                                                                                   |
| `--disable-rfq`            | `false`    | Skip the RFQ book feeds (Bebop, Hashflow, Liquorice, Native)                                                     |
| `--disable-pamm`           | `false`    | Skip the pAMM book feeds (Metric)                                                                                |
| `--rfq-all-pairs`          | `false`    | Publish each RFQ venue as one component for every pair it prices                                                 |
| `--disable-price-level-stream` | `false` | Skip Titan pAMM price level stream testing (only active on Ethereum)                                            |
| `--price-level-stream-block-interval` | `1` | Emit one sampled price level update per this many blocks                                                    |
| `--price-level-stream-stale-threshold-secs` | `10` | Seconds without a Titan message before marking served pAMMs stale in metrics (0 disables)              |
| `--skip-messages-duration` | `600`      | Book stream skip duration (seconds) after processing a message                                                   |
| `--block-wait-time`        | `12`       | Time to wait (seconds) for block N+1 before executing debug_traceCall                                            |
| `--always-test-components` | -          | Comma-separated list of component IDs to test every block                                                        |
| `--max-blocks`             | `0`        | Maximum number of blocks to process before exiting (0 = run indefinitely). When set, prints a summary at the end |
| `--force-catch-up-every-n-blocks` | -   | Run the [catch-up check](#catch-up-check), forcing a resubscription about every this many blocks (at least 50). Off when not set |

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
  --disable-pamm \
  --protocols uniswap_v2 \
  --max-simulations 20 \
  --parallel-simulations 10

# Run with specific components to always test
cargo run --package tycho-integration-test -- \
  --always-test-components component_id_1,component_id_2,component_id_3

# Test low-TVL pools of some protocols without lowering the threshold for the others
cargo run --package tycho-integration-test -- \
  --chain base \
  --protocol-tvl-thresholds lunarbase=10,pancakeswap_infinity_cl=3

# Run for a specific number of blocks and get a summary
cargo run --package tycho-integration-test -- \
  --chain ethereum \
  --max-blocks 10 \
  --protocols uniswap_v2,uniswap_v3
```

## Catch-up check

`--force-catch-up-every-n-blocks N` checks `tycho-client`'s delta catch-up against the live server.
It runs a second stream of the same protocols, with the protocol stream's TVL filter and component
blocklist, next to the tests. About every `N` blocks (at least 50), each of its subscriptions drops
its next 10 messages and ends, so the client resubscribes having missed those blocks. A server with
the delta catch-up endpoint lets the client apply the missed blocks without a snapshot. The client
falls back to a snapshot when the server can't serve them, when a reorg replaced a block the client
had delivered, or when the first message after the gap is a revert: revert messages don't restore
everything yet, so the client doesn't catch up through one. A reorg of blocks the client never
delivered still lets it catch up.

When a protocol has recovered, the check compares the state the client holds for it with a snapshot
of the same components at the same block: component attributes and balances, and the storage,
native balance, code and token balances of their contracts. Every resubscription of the stream is
checked, forced or not, whether the client caught up, took a snapshot or resumed. TVL, tokens and
DCI entry points are not compared.

The state the client holds also carries everything the stream delivered before the gap, and may
already differ from a snapshot there. So when the forced gap is taken, the check keeps a copy of
each protocol's state at that block (the baseline). If the comparison after the recovery finds
differences, it compares the baseline with a snapshot at the baseline's block too. A difference that
both comparisons find exactly (same field, component or contract, key, and the same values on both
sides) counts as `pre_existing`, not as a mismatch. A stale value that changed across the gap, and a
difference that arose in the few blocks between the baseline and the gap, still count as new.

The stream is wired like `TychoStreamBuilder` builds it, with a `DeltasClient` wrapper that ends the
subscriptions; the synchronizers and the catch-up itself are unchanged `tycho-client` code.
Comparisons use an RPC client of their own, so their requests can't delay the synchronizers' catch-up
requests, and each may take at most 5 minutes.

```bash
cargo run --package tycho-integration-test -- \
  --chain ethereum \
  --protocols uniswap_v2,uniswap_v3 \
  --disable-rfq \
  --force-catch-up-every-n-blocks 300
```

### Load and memory

- **Start-up:** the check's stream fetches the protocols' components and an initial snapshot of each
  protocol, like the protocol stream, so the server serves both twice.
- **Each forced gap:** one subscription per protocol resubscribes, then the check fetches a snapshot
  per protocol that tracks components, and a second one at the baseline block when the first finds
  differences. A client that can't catch up also fetches a snapshot per protocol.
- **Memory:** the check holds a second full copy of the tracked state, plus a baseline copy of all of
  it from each forced gap until the protocols recover. Each comparison clones one protocol's state.

### Reading the results

| Metric | Labels | Counts |
|--------|--------|--------|
| `tycho_integration_catch_up_resyncs_total` | `protocol`, `outcome` | How the client recovered from each resubscription: `caught_up` (applied the missed blocks without a snapshot), `snapshot` (fell back to a snapshot), `resumed` (missed no block), `nothing_tracked` (tracks no component, so the two look the same), or `no_resubscription` (did not resubscribe within 5 minutes of a forced gap) |
| `tycho_integration_catch_up_comparisons_total` | `protocol`, `resync`, `result` | Comparisons after each recovery of a protocol that tracks components, judged by the differences that are not pre-existing: `equal`, `known_gaps`, `mismatch`, or `error` (the snapshot could not be fetched in 5 minutes, or the comparison could not be started) |
| `tycho_integration_catch_up_differences_total` | `protocol`, `field`, `status` | Entries that differ from the snapshot, by field and status: `mismatch`, `known_gap` or `pre_existing` |
| `tycho_integration_catch_up_unchanged_recoveries_total` | `protocol` | Recoveries from a forced gap whose held state changed nothing between the baseline and the recovery, so their comparison could not catch a catch-up bug |

- **The check proves nothing unless `outcome="caught_up"` grows.** If only `outcome="snapshot"`
  grows, the client never catches up: the server or its gateway does not serve the catch-up endpoint,
  the server cannot serve the gap, or at every gap a reorg replaced a block the client had delivered
  or the first message after the gap was a revert. `tycho-client` logs the reason at info or warn
  (`Delta catch-up not possible` / `Delta catch-up failed`, target
  `tycho_client::feed::synchronizer`), and the server counts `delta_catch_up_responses` by
  extractor and outcome. A high share of unchanged recoveries also means the gaps prove little.
- **`result="mismatch"` with `resync="caught_up"`** means the state after a catch-up differs from the
  snapshot in a field reverts restore (`status="mismatch"`), at an entry where the baseline did not.
  The check logs it at error with the block's number and hash, and up to 10 of the differing
  entries. The snapshot is fetched by block number, so a reorg between the recovery and the request
  can make it differ; compare the logged hash with the chain's block at that number.
- **`status="known_gap"`** differences are not mismatches:
  - native balances, code and account token balances, which reverts don't restore yet;
  - contracts that only the DCI traces of tracked components reach. Which of them a client holds
    depends on the reorgs it saw, since reverts carry no DCI data. Their storage can differ too:
    `/v1/contract_state` does not filter by protocol system, so a snapshot includes the slots other
    extractors write. A catch-up bug in the storage of such a contract can't show as a mismatch.
- Comparisons after a `snapshot` recovery are a control and should be `equal`.

To see the check's own logs and the client's catch-up decisions, include
`tycho_integration_test=info,tycho_client::feed::synchronizer=info` in `RUST_LOG`.
