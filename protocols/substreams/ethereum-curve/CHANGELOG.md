# Changelog

## v0.3.9

- Update `tycho-substreams` from `0.8.0` to `0.8.1`. Contract changes carrying only
  token balance updates are no longer dropped by `TransactionChangesBuilder` (#1056).
- Remove the dead `SerializableVecBigInt` trait. Its `as_chunks` call required
  Rust 1.88+, breaking builds with the package's pinned 1.83.0 toolchain.

## v0.3.8

- Update `tycho-substreams` from git rev `655fae7` (2025-09-05, pre-0.6.0) to `0.8.0`.
  `get_block_storage_changes` now emits native balance changes in the block storage
  output consumed by the DCI. Earlier builds never emitted them, so native balances
  of DCI-tracked contracts stayed frozen at their initial snapshot.
- Picks up the `previous_value` fix for storage slots written multiple times in one
  transaction, and drops intra-transaction no-op slot changes (tycho-substreams 0.5.1).

## v0.3.7

- Bump the Curve Substreams package version.

## v0.3.6

- Add the Curve Substreams integration.

## v0.3.4

- Initial release.
