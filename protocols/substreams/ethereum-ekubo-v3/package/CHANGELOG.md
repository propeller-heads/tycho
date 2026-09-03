# Changelog

## v0.3.0

- Embed `seed.bin`, a snapshot of every pool at one block written by the package's seed writer
  (`../seed`), and replace that block's events with one synthetic transaction of `PoolSnapshot`
  events so all stores start from the snapshot instead of from protocol genesis. Together with a manifest whose `initialBlock`
  is the seed block this lets an indexer skip the protocol's history. The committed seed is empty
  and pinned to the block before the first pool, so the stock manifest's output is unchanged.

## v0.2.2

- Track the second TWAMM extension deployment
  (`0xd47f1b1edcfeabb08f6ebd8fc337c27e636c75ba`), first used in block 24995117.
  Both deployments are now recognized as TWAMM emitters and as extensions
  carrying time-rate deltas.

## v0.2.1

- Align the `Cargo.toml` package version with the `substreams.yaml` manifest
  version, which has been `v0.2.1` since the CI fix. No behavior change.

## v0.1.3

- Replace the `SIGNED_EXCLUSIVE_SWAP_ADDRESS` placeholder with the deployed
  SignedExclusiveSwap extension address, so `is_exclusive` tagging matches
  real signed pools on-chain.

## v0.1.2

- Tag SignedExclusiveSwap pools with the reserved `is_exclusive` static attribute
  (value `[1u8]`) at component creation, so consumers can keep pools requiring
  off-chain swap authorization out of public routing.

## v0.1.1

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.
