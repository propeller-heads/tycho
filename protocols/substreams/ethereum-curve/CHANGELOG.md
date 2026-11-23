# Changelog

## v0.5.0

- Add the `policy` state attribute to Twocrypto pools: the pool's policy contract, or the zero
  address if it has none. This changes the output.
- Add the `policy_version`, `policy_controller`, `policy_router`, `policy_killed` and
  `policy_min_fee` state attributes to Twocrypto pools: the grant terms of a policy that emits
  `FyndPolicyCreated`, or zero values. This changes the output.

## v0.4.0

- Report a negative balance tally as zero. The tally sums `Transfer` deltas, so it goes negative
  when a pool's rebasing-token balance grew without a `Transfer` and the pool is then drained; the
  two's-complement bytes were read back by Tycho as a huge balance (a drained STRCx/USDC pool
  showed 18.4 STRCx instead of 0). This changes the output for those balances.

## v0.3.10

- Update `tycho-substreams` from `0.8.1` to `0.8.2`. Native balance changes recorded on the
  callee's call frame are now emitted, so the ETH a pool pays out of a swap no longer leaves its
  VM balance stale (#1544).

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
