# Changelog

## v0.3.1

- Update `tycho-substreams` from `0.5.1` to `0.8.0`. `get_block_storage_changes` now
  emits native balance changes in the block storage output consumed by the DCI.
  Earlier builds never emitted them, so native balances of DCI-tracked contracts
  stayed frozen at their initial snapshot.
- Align `Cargo.toml` version (`0.2.0`) with the manifest version (`v0.3.0`); both are
  now `0.3.1`.
