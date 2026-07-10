# Changelog

## v0.3.8

- Update `tycho-substreams` from git rev `655fae7` (2025-09-05, pre-0.6.0) to `0.8.0`.
  `get_block_storage_changes` now emits native balance changes in the block storage
  output consumed by the DCI. Earlier builds never emitted them, so native balances
  of DCI-tracked contracts stayed frozen at their initial snapshot.
- Picks up the `previous_value` fix for storage slots written multiple times in one
  transaction, and drops intra-transaction no-op slot changes (tycho-substreams 0.5.1).
