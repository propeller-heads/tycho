# tycho-simulation

Off-chain DeFi protocol simulation library. Computes swap outputs, spot prices, and price impact
for any protocol indexed by Tycho.

## Key Modules (`src/`)

- **`protocol/`**: Consumer-facing models — `ProtocolComponent`, `Update`, and the crate's error
  types. The `ProtocolSim` trait itself lives in `tycho-common` (`simulation/protocol_sim.rs`)
- **`evm/simulation.rs`**: `SimulationEngine` — runs EVM transactions via `revm`
- **`evm/engine_db/`**: Database backends (`SimulationDB` in-memory, `TychoDB` RPC-backed)
- **`evm/decoder.rs`**: `TychoStreamDecoder` — turns feed snapshots into `ProtocolSim` instances.
  **A new protocol must be registered here** to be decodable
- **`evm/stream.rs`**: Tycho feed integration — wires the decoder onto a live `FeedMessage` stream
- **`evm/pending.rs`**: `TxDeltaIndexer` implementation — replays in-flight blocks to produce
  pending-state deltas
- **`evm/protocol/filters.rs`**: Public pool filters consumers pass when registering protocols
- **`evm/override_stream/`**: live per-block VM state overrides for pAMMs — generic
  `StateOverrideProvider`/`OverrideSnapshot` core plus the Titan quote-stream provider; pools
  resolve the latest snapshot on every simulation and can fall back to indexed state per the
  snapshot's `FailurePolicy`
- **`evm/protocol/`**: Protocol implementations
  - **Native** (`uniswap_v2/`, `uniswap_v3/`, `uniswap_v4/`, `ekubo/`, `ekubo_v3/`, `cowamm/`,
    `aerodrome_v1/`, `aerodrome_slipstreams/`, `velodrome_slipstreams/`, `pancakeswap_v2/`,
    `ramses_v3/`, `ring_swap_v2/`, `lunarbase/`, `native_wrapper/`, `sky/`, `etherfi/`,
    `erc4626/`, `rocketpool/`): Pure Rust math, no EVM execution.
    `cpmm.rs` / `clmm.rs` / `safe_math.rs` / `u256_num.rs` / `utils.rs` are shared math helpers,
    not protocols
  - **Hybrid** (`fluid/`, `balancer_v3/`, `curve/`): native Rust quote math over VM-indexed pool
    state (each has both `state.rs` and `vm.rs`)
  - **VM** (`vm/`): Generic Solidity adapter (`TychoSimulationContract`) executed in `revm` for
    protocols without a native implementation
- **`rfq/`**: RFQ clients for off-chain market makers (`rfq/protocols/`: `bebop`, `hashflow`,
  `liquorice`, `metric`). Only Bebop streams over WebSocket; the rest poll over HTTP
- **`price_level_stream/`**: Titan pAMM price level stream — `PriceLevelStreamBuilder` turns the
  Titan WebSocket's per-pair quote-ladder frames directly into `Update`s (no indexer feed
  round-trip); `PriceLevelStreamState` quotes by interpolating the ladder and refuses to quote
  once its frame is one block time old (`quotable_until`, monotonic, never serialized). Frames
  are best-effort, not complete snapshots: `tracker.rs` keys freshness per component, never
  removes on frame diff, and emits `removed_pairs` only when a component's data is `stale_after`
  (default 24 s) old or its PropAMMRouter family changes; the next accepted frame carrying it
  re-adds it. Frames are accepted only if their wire `timestamp` is younger than `stale_after`,
  not more than one slot in the future, not older than the newest accepted one (equal allowed),
  and their block neither regresses nor jumps implausibly; the block frontier resets whenever
  nothing is served.
  `build()` is an `async_stream` loop selecting over frames, an earliest-deadline timer, and the
  whitelist reader (`fallback_router.rs`, each read bounded by 15 s, retried with backoff,
  refreshed every 10 min); nothing is served until the whitelist is known, and without
  `fallback_router_rpc_url` or `RPC_URL` nothing is ever served. `titan.rs` counts only parsed
  frames as liveness (idle timeout 10 s). `telemetry.rs` emits `price_level_stream_*` metrics via
  the `metrics` facade, per-venue series pre-initialised to zero, no wire values as labels except
  the address of an auto-detected venue. Components are identified as `pricelevelstream:{pamm}` or
  `propammfallback:{pamm}` for
  whitelisted venues; a stale expiry and a retired venue look the same downstream. Registration
  precedence (`with_known_pamms`, `add_pamm`, `deny_pamm`, auto-detection) is unchanged: later
  explicit calls win, defaults never override them.

## Simulation Approaches

**Always prefer native.** If a protocol's behaviour can be ported to Rust, it should be. VM is a
fallback for protocols too complex to port, not a default.

1. **Native** — pure Rust math; fastest. Use whenever the protocol logic can be expressed in Rust.
2. **Hybrid** — native Rust math for swap calculation, but reads/updates pool state via the local
   VM (`SimulationDB`). Use when the swap logic can be ported but state is complex to track
   independently. Examples: Fluid V1, Balancer V3, Curve. Note that a hybrid protocol keeps its
   VM-shaped indexing — component keys stay `vm:*` and the indexer still tracks full contract
   storage; only the quote path changes.
3. **VM** — Solidity adapter in `revm`; works for any EVM protocol but is slower and requires an
   adapter contract in `protocols/adapter-integration/`. Use only when native is not feasible.
4. **RFQ** — off-chain quotes via API; for protocols that cannot be simulated on-chain at all.

## Pending-block state for hybrid/VM protocols

`apply_deltas_ephemeral` applies only `state_deltas`; nothing on the pending path writes to the
VM database, so `apply_deltas_ephemeral` can't read the pending state from there. A protocol
whose `delta_transition` re-reads the VM would therefore quote a pending block against confirmed
state. Fluid and Curve close that gap the same way:

1. A `TxDeltaIndexer` implementation — which lives in the consuming repo, not here — builds
   `evm::simulation::PendingOverrides` (storage, native balances and block environment) from the
   accounts a `PendingBlock` carries.
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

- CI pins a nightly toolchain for both `fmt` and `clippy` (see `.github/workflows/ci-rust.yaml`);
  stable for builds and tests
- `rstest`: name each parametrised case with `#[case::descriptive_name(...)]`
- Mark every test that hits external services `#[ignore = "Requires RPC_URL ..."]`. CI runs
  `--all-features`, so `#[cfg_attr(not(feature = "network_tests"), ignore)]` does not exclude the
  test and it fails without `RPC_URL`
