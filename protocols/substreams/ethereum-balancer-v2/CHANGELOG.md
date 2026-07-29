# Changelog

## v0.5.0

- Ensure Balancer V2 swap fees are exported as dynamic attributes instead of static.
  Adds an event listener for `SwapFeePercentageChanged` so `fee` correctly tracks
  admin changes made after pool creation.

## v0.4.4

- Update the package version for the release.

## v0.4.3

- Update `tycho-substreams` from `0.8.0` to `0.8.1`. Contract changes carrying only
  token balance updates are no longer dropped by `TransactionChangesBuilder` (#1056).

## v0.4.2

- Update `tycho-substreams` from `0.5.1` to `0.8.0`. `get_block_storage_changes` now
  emits native balance changes in the block storage output consumed by the DCI.
  Earlier builds never emitted them, so native balances of DCI-tracked contracts
  stayed frozen at their initial snapshot.

## v0.4.1

- Initial release.
