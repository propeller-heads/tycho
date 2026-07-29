# Changelog

## v0.4.3

- Update `tycho-substreams` from `0.8.0` to `0.8.1`. Contract changes carrying only
  token balance updates are no longer dropped by `TransactionChangesBuilder` (#1056).
  The vault regularly nets storage writes out to no-ops while token balances still
  change, so those balance updates were silently lost with `0.8.0`.

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

## v0.4.0

- Initial release.
