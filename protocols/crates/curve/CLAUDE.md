# curve-core

Native Curve processor: the Curve deltas a pending block would produce, without a Substreams
runtime. Implements `tycho_common::traits::TxDeltaIndexer`.

## Why Curve differs from the UniswapV2/V3/V4 processors

Those protocols describe their state in their logs, so a processor can decode a pending block from
`TxInput`s alone. Curve is a VM protocol: its state lives in contract storage. Two consequences
shape this crate.

1. **Pool state comes from view getters, not logs.** The processor builds storage, native-balance
   and block-environment overrides from the accounts a `PendingBlock` carries, then reads each
   affected pool's getters under them via `tycho_simulation`'s
   `evm::protocol::curve::read_pool_readings`. The readings travel to `CurveState` as the
   `pool_state_adjusted` state-delta attribute, because `apply_deltas_ephemeral` applies only
   `state_deltas` — nothing on the pending path patches the VM database.
2. **The block environment must be the pending block's own.** A ramping `A()` and the rate
   providers behind `stored_rates()` interpolate against the block timestamp, so reading them under
   the parent block's clock misprices the pool.

Component balances take the log route and are a 1:1 port of the substreams
`map_relative_balances` / `store_balances` pair, so they can be compared byte for byte against the
package.

## Modules

| File | Content |
|---|---|
| `processor.rs` | `CurveProcessor`, the `TxDeltaIndexer` impl. `shared()` reads the indexed VM storage the stream decoder fills; `with_engine()` takes any engine |
| `registry.rs` | Tracked pools (id, address, tokens, coin count, variant) and the reverse index from an address to the pools it affects |
| `balance.rs` | Absolute component balances: transfer logs applied to a running total seeded from the confirmed stream |
| `overrides.rs` | `PendingBlock` accounts → `PendingOverrides` |

## Conventions worth keeping

- **Balances are signed and unclamped.** The Curve substreams emit `to_signed_bytes_be()` of the
  store value, unlike `tycho_substreams::balances::aggregate_balances_changes`, which clamps
  negatives. Clamping here would change the bytes.
- **Old sUSD transfers credit the new sUSD token**, matching the substreams' token substitution.
- **The ETH sentinel `0xEee…EeE` normalizes to the zero address**, as the emitted component tokens
  do.
- **Native ETH balances come from the account, not from logs.** The substreams derive them from
  per-call balance changes in the block trace, which a `PendingBlock` does not carry.
- **A pool that fails to read is dropped with a warning**; the rest of the block still produces
  deltas.

## Known gaps

- Pools created inside a pending block are not emitted. The registry only advances on confirmed
  blocks, matching what the indexer serves downstream.
- Entrypoints (DCI) are not produced for pending blocks; they only fire on new components and
  admin changes.
- A pool is pulled in by a change to its own storage, to one of its component contracts, or to its
  base pool. A change confined to some other contract its getters read — an unindexed rate
  provider, say — does not trigger a re-read.

## Testing

`cargo test -p curve-core --lib` needs nothing external.

The parity test is behind the `parity-test` feature, which pulls `tycho-indexer` for the
substreams client and therefore needs Postgres client libraries to link:

```bash
protocols/crates/curve/scripts/build_main_spkg.sh
TOKEN=$(curl -s https://auth.streamingfast.io/v1/auth/issue \
  -d "{\"api_key\":\"$STREAMINGFAST_KEY\"}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')
STREAMINGFAST_KEY="$TOKEN" cargo test -p curve-core --features parity-test \
  --test integration -- --ignored --nocapture
```

It checks balances against the spkg built from `origin/main` and pool state against the chain, and
fails if the block window yielded no comparisons. Window overrides: `CURVE_START_BLOCK`,
`CURVE_STOP_BLOCK`, `CURVE_SEED_BLOCKS`.
