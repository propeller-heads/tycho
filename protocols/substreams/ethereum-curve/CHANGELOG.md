# Changelog

## v0.4.0

- StableSwap NG metapool components list the coins of their base pool after their own two, and
  carry a new `underlying_coins` static attribute: the metapool's coin first, then the base pool's
  coins in the base pool's order, the index order `exchange_underlying` takes. Consumers can then
  quote and encode swaps between the metapool's coin and the base pool's coins. Component tokens
  are set at creation, so existing deployments need a resync to pick this up.

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
