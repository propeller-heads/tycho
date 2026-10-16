# Changelog

## v0.2.0

- Update indexed state for LunarBase v0.4 punishment: track maximum punishment,
  fee changes from `PunishmentApplied`, and the complete uint160 anchor price.
  This changes the state schema and requires the matching simulator using
  `lunarbase-pmm-math` 0.4.1 and a fresh indexing replay.
- Always quote Tycho partner trades with fee multiplier one while retaining
  directional fees and punishment; there is no caller-policy configuration. Remove
  router identity and caller fee policy from Substreams parameters and state, so a router
  change does not require reindexing.
- Lower the simulator's routing gas estimate to 100,000 using the PR review's
  reported Base/BSC pool-call measurements.
- Add a BNB Smart Chain manifest sharing the existing WASM for the native BNB/USDT pool.
- Seed Base at block 51297755 and BSC at block 121835899 with complete snapshots
  of their parent blocks. Validate parent number/hash and all nine attributes;
  seed active reserve balances before applying events. Retain RPC capture provenance
  and historical caller policy separately from the indexed state.
- Accept optional `bootstrap_states` JSON in module parameters for verified
  snapshots when indexing an existing pool.
- Add a separate Base execution fixture for a block-pinned partner-router range.
  Keep the production bootstrap and original captures unchanged; the shared test
  harness needs no caller-policy options or pool storage overrides.
- Document bootstrap completeness, the fixed partner model, and validation and
  execution registration requirements.

## v0.1.3

- Switch `tycho-substreams` from a path dependency on the in-tree crate to the
  published `0.8.1`, so releases build against a fixed, published version.

## v0.1.2

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.
