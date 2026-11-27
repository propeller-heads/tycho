# Changelog

## v0.5.1

- Add QuantAMM weighted pool support via the `QuantAMMWeightedPoolFactory` (new
  optional `quantamm_factory` deployment parameter) on Ethereum mainnet and Base.
  Arbitrum and Gnosis manifests omit the param (defaults to empty). Pool creates
  from both `create` and `createWithoutArgs` are indexed. Static attributes are
  `pool_type`, `fee`, and optional `rate_providers` — create-time weights are
  omitted because QuantAMM weights change via pool storage and time interpolation.

## v0.5.0

- Add reCLAMM pool support via the `ReClammPoolFactory` (new `reclamm_factory`
  deployment parameter).
- Derive pool token balances from the Vault's `_poolTokenBalances` storage writes
  instead of the amounts carried by `Swap`/`LiquidityAdded`/`LiquidityRemoved`
  events. Event amounts miss fee, hook, and rounding adjustments that are
  already reflected in the final storage write. Balances are reported as
  absolute values straight from storage, so no relative-delta accounting is
  needed and a missed write is corrected by the next observed one.
- Add deployment manifests for Arbitrum (`arbitrum-balancer-v3.yaml`),
  Base (`base-balancer-v3.yaml`), and Gnosis (`gnosis-balancer-v3.yaml`).
- Add the `skip_rate_provider_pools` deployment parameter to exclude pools
  configured with rate providers.
- Remove the `manual_updates` static attribute from pools.
- Store the wrapped-to-underlying buffer token mapping with a
  set-if-not-exists policy so the first registration wins.

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
