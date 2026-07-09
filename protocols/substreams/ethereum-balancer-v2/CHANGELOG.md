# Changelog

## v0.4.2

- Update `tycho-substreams` from `0.5.1` to `0.8.0`. `get_block_storage_changes` now
  emits native balance changes in the block storage output consumed by the DCI.
  Earlier builds never emitted them, so native balances of DCI-tracked contracts
  stayed frozen at their initial snapshot.
