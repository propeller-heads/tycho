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
- **`price_level_stream/`**: Titan pAMM price level stream — `PriceLevelStreamBuilder` turns the
  Titan WebSocket's per-pair quote-ladder snapshots directly into `Update`s (no indexer feed
  round-trip); `PriceLevelStreamState` quotes by interpolating the ladder. Components are
  identified as `pricelevelstream:{pamm}`. A new builder serves nothing: `with_known_pamms`
  registers the known-good venues and denies known-unexecutable ones, `add_pamm` registers
  individual ones, `deny_pamm` excludes one (dropping any registration and blocking
  auto-detection), and opt-in auto-detection additionally serves unknown venues under their
  address (`pricelevelstream:{0xaddress}`). Precedence: between `add_pamm` and `deny_pamm` for
  the same address the later call wins; `with_known_pamms` defaults never override either,
  regardless of call order. `build` emits venues on Titan's PropAMMRouter whitelist under
  `propammfallback:{pamm}` instead, so tycho-execution routes their swaps through the router
  (Uniswap V3 fallback on venue revert); it reads that whitelist once on the first poll via
  `RPC_URL`, and warns and stays on the direct path without it. `without_fallback_router` skips
  the read and keeps every venue on the direct path. Venues may overlap with other integration
  paths of the same liquidity (e.g. `vm:fermiswap`) — consumers must deduplicate by venue where
  double-counting matters

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

## Pending-block state for hybrid/VM protocols

`apply_deltas_ephemeral` applies only `state_deltas`, so nothing on the pending path writes to the
VM database. A protocol whose `delta_transition` re-reads the VM would therefore quote a pending
block against confirmed state. Fluid and Curve close that gap the same way:

1. A native processor (`protocols/crates/<protocol>`) builds `evm::simulation::PendingOverrides`
   — storage, native balances and block environment — from the accounts a `PendingBlock` carries.
2. It reads the protocol's state under those overrides (`fluid::call_resolver`,
   `curve::read_pool_readings`) and puts the result in a state-delta attribute
   (`pool_reserves_adjusted`, `pool_state_adjusted`).
3. `delta_transition` branches on that attribute and rebuilds from it, falling back to the VM read
   when it is absent.

Reading under the pending block's own number and timestamp matters: anything with on-chain time
math (Fluid's expanding limits, Curve's ramping `A()`) is wrong under the parent block's clock.

## Features

| Feature | Default | Contents |
|---------|---------|----------|
| `evm` | yes | `revm`, `SimulationEngine`, all EVM protocol impls |
| `rfq` | yes | RFQ WebSocket client and protocol adapters |
| `price-level-stream` | yes | Titan pAMM price level stream client |
| `network_tests` | no | Gates tests that require live network access |

## Conventions

- `cargo +nightly fmt` for formatting; stable toolchain for everything else
- `rstest`: name each parametrised case with `#[case::descriptive_name(...)]`
- Mark every test that hits external services `#[ignore = "Requires RPC_URL ..."]`. CI runs
  `--all-features`, so `#[cfg_attr(not(feature = "network_tests"), ignore)]` does not exclude the
  test and it fails without `RPC_URL`
