# ethereum-ekubo-v3-seed

Seed writer for the `ethereum-ekubo-v3` package in `../package`; see `crates/tycho-seed/README.md`
for what a seed is and how a package uses it.

```sh
# RPC_URL must point at an archive node that supports debug_storageRangeAt.
cargo run --release -p ethereum-ekubo-v3-seed -- rpc --block 25895000 --out ekubo-v3-25895000.seed.bin
cargo run --release -p ethereum-ekubo-v3-seed -- inspect ekubo-v3-25895000.seed.bin

# The empty seed the package embeds by default (block defaults to the one before the first pool).
cargo run --release -p ethereum-ekubo-v3-seed -- empty --out protocols/substreams/ethereum-ekubo-v3/package/seed.bin
```

## The `rpc` source

1. `PoolInitialized` logs of the core from the protocol's first block to the seed block name every
   pool and its config.
2. `debug_storageRangeAt` dumps the full storage of the core and of the TWAMM v1, TWAMM v2 and
   BoostedFees extensions after the seed block.
3. Every slot is attributed to a pool arithmetically: the contracts address pool state, ticks,
   bitmaps and time infos as `poolId + OFFSET + key` (`CoreStorageLayout.sol`,
   `TWAMMStorageLayout.sol`), so subtracting the offset and finding the closest pool id below
   recovers the key.
4. Per pool: current tick, sqrt ratio and liquidity from the state slot; ticks with a nonzero net
   liquidity delta (concentrated pools) or the single fixed-range position (full-range and stableswap
   pools); rates, last execution time and pending rate deltas for timed pools; token reserves implied
   by the liquidity distribution.
5. Consistency checks between logs, storage and bitmaps abort the run unless `--lenient` is passed.

The seed's balances are liquidity-implied reserves. The stock package's balances are sums of swap and
position deltas, which include fees that were later collected, so the two differ by design.

## Verifying a seed

1. Dump `/v1/protocol_state` (with balances) and `/v1/protocol_components` for `ekubo_v3` at
   block `N + k` from the reference instance.
2. Reset the database, build the seeded package with `protocols/substreams/pack-seeded.sh`, set the
   extractor's `start_block` to exactly `N`, sync past `N + k` and dump the same endpoints.
3. `scripts/compare-ekubo-seed.py` compares per component: tokens and static attributes must match;
   entity attributes compared as integers (`tick/*`, `rate_delta/*` and `liquidity` signed,
   everything else unsigned, an empty value is zero); the stock run's `rate_delta/*/t` entries with
   `t <= last_time`, which the chain has already consumed, are dropped; `created_at` and
   `creation_tx` differ on purpose; balances are reported as relative deviations.
4. `scripts/validate_ticks_state.py` checks that every pool's tick deltas sum to zero.
