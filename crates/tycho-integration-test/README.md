# Tycho Integration Test

A comprehensive integration test that validates the Tycho system by performing end-to-end testing.
This test runs continuously in the cluster but it can also be run locally for debugging.

1. **Stream Processing**: Connects to Tycho indexer and receives live protocol state updates
2. **State Validation**: For each protocol update (after the first one) and for each swap direction:
    1. **Get Limits**: Retrieves maximum input/output limits for token pairs
    2. **Simulate Swap**: Calls `get_amount_out` with 0.1% of max input amount
    3. **Encode Transaction**: Builds the actual swap transaction via TychoRouter
    4. **Execute Simulation**: Runs `debug_traceCall` with necessary state overrides
    5. **Calculate Slippage**: Compares simulated vs actual execution results

## Supported Protocols

- **On-chain Protocols**: All DEX protocols supported by Tycho (Uniswap V2/V3, Curve, Balancer, etc.)
- **RFQ Protocols**: Request-for-Quote protocols like Hashflow

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
| `--skip-messages-duration` | `600`      | RFQ stream skip duration (seconds) after processing a message                                                    |
| `--block-wait-time`        | `12`       | Time to wait (seconds) for block N+1 before executing debug_traceCall                                            |
| `--always-test-components` | -          | Comma-separated list of component IDs to test every block                                                        |
| `--max-blocks`             | `0`        | Maximum number of blocks to process before exiting (0 = run indefinitely). When set, prints a summary at the end |
| `--router-fee`             | `10`       | Router fee on output in bps (must be ≥ 1). Deducted from the executed amount before slippage is calculated       |

## Running Locally

```bash
# Set required environment variables
export RUST_LOG=tycho_integration_test=info,tycho_test=debug,error
export TYCHO_URL=tycho-dev.propellerheads.xyz
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
