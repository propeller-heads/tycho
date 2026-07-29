# Changelog

## v0.2.0

### Changed

- Follow the Fermi engine migration of 2026-07-21: `engine_address` now points at the new engine
  `0x90f73fEA1Ee2Dc514d4dbAc0bfF7ff04b933767f` (deployed block 25581530). Fermi migrates engines
  by re-pointing the swapper's engine slot (re-point tx at block 25581704); with the old address
  the package missed pair registrations and pause events on the new engine, no longer matched the
  registry `updateState` calls that feed the `override_block_timestamp` attribute, and simulation
  failed with `MissingAccount(<new engine>)` because the new engine's storage was never indexed.
- Retarget the integration tests to the new engine's history. Stop blocks are constrained by the
  engine's oracle staleness window (~30s, `StaleUpdate`): state decoding snapshots at the stop
  block and quotes through the adapter, so the stop block must come after the swapper re-point and
  the first `updateState` for every tested lane. Execution checks are enabled (the `vm:fermiswap`
  executor is registered and deployed).
- Deduplicate the manifest: single anchored `initialBlock` and params entry instead of the
  per-module `networks` map. `initialBlock` stays at the trader-vault creation block 24936662 — it
  must remain at or before the creation of every tracked contract, because account rows are only
  created for in-range deployments and storage deltas against absent accounts kill the extractor.

### Migration

Requires a full resync of the `vm:fermiswap` extractor. Existing components carry the old engine
in their contract set and cannot be patched forward; the new engine re-registered all pairs, so
components are re-created (same component ids — `keccak(base ++ quote)` is engine-independent).
Stale components from the old engine must be wiped, not left in place: Fermi poisons the outgoing
engine with garbage prices during wind-down (last old-engine update quoted WETH at 9.53 USDC).

The engine address is also hardcoded as `FERMISWAP_TARGET_ADDRESS` in
`crates/tycho-test/src/execution/encoding.rs` (updated together with this release) — it must be
kept in sync with `engine_address` on every future migration, or the execution harness's lane
staleness overwrite patches a dead registry slot.

## v0.1.2

- Switch `tycho-substreams` from a path dependency on the in-tree crate to the
  published `0.8.1`, so releases build against a fixed, published version. The
  package relies on the token balance retention fix (#1056) included in `0.8.1`:
  the trader vault contract change often carries only token balance updates,
  which `0.8.0` dropped as empty.

## v0.1.1

- Isolate self-contained token proxies in the shared database.

## v0.1.0

- Add FermiSwap pair indexing with protobuf-backed stores.
