# Changelog

## v0.4.2

- Pin the Rust toolchain to 1.96.0 for reproducible wasm builds. The package
  previously had no toolchain pin and built with whatever stable was current.

## v0.4.1

- Update `tycho-substreams` from git rev `51995f9` (2025-06-05, pre-0.6.0) to `0.8.0`.
  `get_block_storage_changes` now emits native balance changes in the block storage
  output consumed by the DCI. Earlier builds never emitted them, so native balances
  of DCI-tracked contracts stayed frozen at their initial snapshot.
- Picks up the `previous_value` field and its multi-write fix for storage slot
  changes (tycho-substreams 0.5.0/0.5.1).
