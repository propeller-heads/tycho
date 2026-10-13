# arbitrum-camelot-v3

Substreams package indexing [Camelot V3](https://docs.camelot.exchange/) on Arbitrum One as a
Tycho native integration (`camelot_v3`, component type `camelot_v3_pool`), simulated by
`CamelotV3State` in `tycho-simulation`.

Camelot V3 is an Algebra V1.9 deployment: Uniswap V3 concentrated liquidity over a tick table
without spacing compression, with a directional adaptive fee (`feeZto` / `feeOtz`). The fee is
recomputed by the first swap or in-range liquidity change at a new block timestamp, from the
1-day volatility and volume averages of an oracle ring kept by a per-pool `DataStorageOperator`.
The package emits everything that computation and the swap loop read as state attributes,
decoded from the storage words the pool and its operator write.

## Contracts

| Contract | Address | Role |
| --- | --- | --- |
| `AlgebraFactory` | `0x1a3c9B1d2F0529D97f2afC5136Cc23e58f1FD35B` | Emits `Pool`; writes a new pool's fee configuration into its operator |
| `AlgebraPool` | one per pair, from the `Pool` event | The pool: `globalState`, `liquidity`, `ticks` |
| `DataStorageOperator` | one per pool, created by the factory in `createPool` | Timepoint ring and fee configurations |

The operator address is not part of any event. `createPool` deploys it with `CREATE` right before
the pool deployer deploys the pool, so the package recovers it from the transaction trace as the
single contract created directly by the factory call that emitted `Pool`, and errors if that
shape does not hold. It is exposed as the `data_storage_operator` static attribute.

## Modules

| Module | Output |
| --- | --- |
| `map_protocol_components` | One `camelot_v3_pool` component per `Pool` event with tokens `[token0, token1]` and the `data_storage_operator` static attribute |
| `store_protocol_components` | Components keyed by pool address and by operator address |
| `map_relative_component_balances` | Pool token balance deltas from ERC20 `Transfer` events, both sides handled independently |
| `store_balances` | Absolute pool balances |
| `store_operator_slots` | Latest value of every storage word a tracked operator wrote |
| `map_protocol_changes` | `BlockChanges`: components with their initial attributes, balances, and the state attributes below |

All modules start at the factory creation block `101163738`.

## State attributes

Values are big-endian and fixed-width, cut from the storage word they live in.

| Attribute | Source | Encoding |
| --- | --- | --- |
| `sqrt_price_x96`, `tick`, `fee_zto`, `fee_otz`, `timepoint_index` | pool slot 2 (`globalState`) | uint160, int24, uint16, uint16, uint16 |
| `liquidity`, `volume_per_liquidity_in_block` | pool slot 3 | uint128, uint128 |
| `ticks/{tick}` | pool `ticks[tick]`, for the ticks a `Mint` or `Burn` names | int128 `liquidityDelta`; deleted when `liquidityTotal` drops to zero, so a zero delta keeps the tick |
| `timepoints/{index}` | operator slots `2 * index` and `2 * index + 1` | the two 32-byte words of `timepoints[index]` |
| `fee_config_zto`, `fee_config_otz` | operator slots `131072` and `131073` | the 32-byte word holding `AdaptiveFee.Configuration` |

Balances come from `Transfer` events rather than pool events: a swap forwards the community
share of its fee to the factory's vault inside the same call, and neither that amount nor the
vault is part of the `Swap` event.

### Pruning the timepoint ring

The ring holds 65,536 entries and busy pools have wrapped it, but the fee only reads the oldest
entry, the last two, and the pair around `now - 1 day`. Timestamps grow along the ring and
execution time only moves forward, so once an entry is older than the newest timepoint at or
before `now - 1 day` (and is not the one right before the last), nothing reads it again. On
every write the package deletes the entries that the previous write still kept and this one no
longer needs, using `store_operator_slots` to read timestamps by ring index. A busy pool's
snapshot therefore carries about a day of timepoints instead of the whole ring.

## Build and test

```bash
# From protocols/substreams
cargo build --package arbitrum-camelot-v3 --target wasm32-unknown-unknown --release
cargo test --package arbitrum-camelot-v3

# From protocols/testing, with an Arbitrum One archive RPC in RPC_URL
cargo run -- range --package arbitrum-camelot-v3 --chain arbitrum
```
