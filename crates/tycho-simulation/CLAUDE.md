# tycho-simulation

Off-chain DeFi protocol simulation library. Computes swap outputs, spot prices, and price impact
for any protocol indexed by Tycho.

## Key Modules (`src/`)

- **`protocol/`**: Core traits and models — `ProtocolSim`, `ProtocolComponent`, `Update`
- **`evm/simulation.rs`**: `SimulationEngine` — runs EVM transactions via `revm`
- **`evm/engine_db/`**: Database backends (`SimulationDB` in-memory, `TychoDB` RPC-backed)
- **`evm/stream.rs`**: Tycho feed integration — decodes live `FeedMessage` state into protocol
  instances ready for simulation
- **`evm/override_stream/`**: live per-block VM state overrides for pAMMs — generic
  `StateOverrideProvider`/`OverrideSnapshot` core plus the Titan quote-stream provider; pools
  resolve the latest snapshot on every simulation and can fall back to indexed state per the
  snapshot's `FailurePolicy`
- **`evm/protocol/`**: Protocol implementations
  - **Native** (`uniswap_v2/`, `uniswap_v3/`, `uniswap_v4/`, `ekubo/`, `cowamm/`, `fluid/`,
    `aerodrome_v1/`, `aerodrome_slipstreams/`, `pancakeswap_v2/`, `etherfi/`, `erc4626/`,
    `rocketpool/`, `cpmm/`, `clmm/`): Pure Rust math, no EVM execution
  - **VM** (`vm/`): Generic Solidity adapter (`TychoSimulationContract`) executed in `revm` for
    protocols without a native implementation
- **`rfq/`**: RFQ client for off-chain market makers (WebSocket-based quote streaming)

## Simulation Approaches

**Always prefer native.** If a protocol's behaviour can be ported to Rust, it should be. VM is a
fallback for protocols too complex to port, not a default.

1. **Native** — pure Rust math; fastest. Use whenever the protocol logic can be expressed in Rust.
2. **Hybrid** — native Rust math for swap calculation, but reads/updates pool state via the local
   VM (`SimulationDB`). Use when the swap logic can be ported but state is complex to track
   independently. Example: Fluid V1.
3. **VM** — Solidity adapter in `revm`; works for any EVM protocol but is slower and requires an
   adapter contract in `protocols/adapter-integration/`. Use only when native is not feasible.
4. **RFQ** — off-chain quotes via API; for protocols that cannot be simulated on-chain at all.

## Features

| Feature | Default | Contents |
|---------|---------|----------|
| `evm` | yes | `revm`, `SimulationEngine`, all EVM protocol impls |
| `rfq` | yes | RFQ WebSocket client and protocol adapters |
| `network_tests` | no | Gates tests that require live network access |

## Conventions

- `cargo +nightly fmt` for formatting; stable toolchain for everything else
- `rstest`: name each parametrised case with `#[case::descriptive_name(...)]`
- `network_tests` feature gates any test that hits external services — do not leave network calls
  in tests without this gate
