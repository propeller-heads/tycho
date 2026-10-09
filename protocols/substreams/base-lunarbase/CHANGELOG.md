# Changelog

## v0.2.0

- Update indexed state for LunarBase v0.4 punishment: track maximum punishment,
  fee changes from `PunishmentApplied`, and the complete uint160 anchor price.
  This changes the state schema and requires the matching simulator using
  `lunarbase-pmm-math` 0.4.1 and a fresh indexing replay.
- Require a `quote_caller` parameter and track its whitelist status alongside the
  pool's blacklist fee multiplier. Quotes now apply the configured caller's policy
  instead of assuming multiplier one. Replaying state is required when changing
  callers or migrating snapshots that lack these attributes.
- Add a BNB Smart Chain manifest sharing the existing WASM, with the native
  BNB/USDT pool and its configured Tycho quote caller.
- Seed Base at block 51297755 and BSC at block 121835899 with complete,
  caller-bound snapshots of their parent blocks. Validate parent number/hash and
  all eleven attributes; seed
  active reserve balances before applying events. Include RPC capture provenance
  and observed quotes, and enable simulation/execution for both modern ranges.
- Accept optional `bootstrap_states` JSON in module parameters for verified
  snapshots when indexing an existing pool. Changing the caller also requires
  replacing its bootstrap snapshot.
- Document bootstrap completeness, caller fee policy, and the validation and
  execution registration needed before enabling the new deployment.

## v0.1.3

- Switch `tycho-substreams` from a path dependency on the in-tree crate to the
  published `0.8.1`, so releases build against a fixed, published version.

## v0.1.2

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.
