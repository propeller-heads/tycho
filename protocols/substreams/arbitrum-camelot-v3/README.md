# arbitrum-camelot-v3

Substreams package indexing [Camelot V3](https://docs.camelot.exchange/) on Arbitrum One as a
Tycho VM integration (`vm:camelot_v3`, component type `camelot_v3_pool`).

Camelot V3 is an Algebra V1.9 deployment: concentrated liquidity with a directional adaptive fee
(`feeZto` / `feeOtz`) computed by a per-pool `DataStorageOperator`, and a tick spacing the factory
owner can change after deployment. None of that is re-implemented here: the package indexes the
code and storage of every contract a swap touches, and simulation runs the pool bytecode itself.

## Contracts

| Contract | Address | Role during a swap |
| --- | --- | --- |
| `AlgebraFactory` | `0x1a3c9B1d2F0529D97f2afC5136Cc23e58f1FD35B` | Emits `Pool`; read for `vaultAddress` when a community fee is paid |
| `AlgebraPool` | one per pair, from the `Pool` event | The pool |
| `DataStorageOperator` | one per pool, created by the factory in `createPool` | Oracle timepoints and adaptive fee |
| `AlgebraPoolDeployer` | `0x6Dd3FB9653B10e806650F107C3B5A0a6fF974F65` | Deploys pools; not called during swaps, not indexed |
| Community vault | `0x58095979B412a366687cA05CbE85fF56241bE21f` | Receives community fee transfers; no code executed, not indexed |

The operator address is not part of any event. `createPool` deploys it with `CREATE` right before
the pool deployer deploys the pool, so the package recovers it from the transaction trace as the
single contract created directly by the factory call that emitted `Pool`, and errors if that
shape does not hold.

## Modules

| Module | Output |
| --- | --- |
| `map_protocol_components` | One `camelot_v3_pool` component per `Pool` event with tokens `[token0, token1]`, contracts `[pool, operator, factory]` and static attribute `data_storage_operator` |
| `store_protocol_components` | Components keyed by pool address and by operator address |
| `map_relative_component_balances` | Pool token balance deltas from ERC20 `Transfer` events, both sides handled independently |
| `store_balances` | Absolute pool balances |
| `map_protocol_changes` | `BlockChanges`: components, balances, code and storage of pools, operators and the factory, `update_marker` for pools whose pool or operator changed, and the `active_incentive` state attribute from `Incentive` events |

All modules start at the factory creation block `101163738` so the factory is witnessed.

## Known limitation

A pool with a non-zero `activeIncentive` calls that virtual pool on every swap and tick crossing.
Those contracts are not indexed, so such pools cannot be simulated. The latest `Incentive` event
value is exposed as the `active_incentive` state attribute for consumers to filter on. The
factory's `farmingAddress` (the only account allowed to set an incentive) is the zero address at
the time of writing.

## Build and test

```bash
# From protocols/substreams
cargo build --package arbitrum-camelot-v3 --target wasm32-unknown-unknown --release
cargo test --package arbitrum-camelot-v3

# From protocols/testing, with an Arbitrum One archive RPC in RPC_URL
cargo run -- range --package arbitrum-camelot-v3 --chain arbitrum
```
