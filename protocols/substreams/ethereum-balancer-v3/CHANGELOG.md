# Changelog

## v0.6.0

- Add StableSurge pool support via the `StableSurgePoolFactory` (optional
  `stable_surge_factory` and `stable_surge_hook` deployment parameters, set
  together). The hook is added to each such pool's contracts and its storage is
  tracked, since it holds the per-pool surge threshold and maximum fee.
- `weighted_factory`, `stable_factory` and `stable_surge_factory` accept a
  comma-separated list, so several generations of a family (same `create`
  signature) are indexed by one manifest. Single addresses parse as before.
- Add the Monad manifest (`monad-balancer-v3.yaml`, Vault
  `0xbA1333333333a1BA1108E8412f11850A5C319bA9`, `initialBlock` 22091249 at the
  Vault's deployment; first `PoolRegistered` at 48702459). Weighted v1+v2, Stable
  v2+v3, StableSurge v2+v3 and reCLAMM v3 factories. Rate-provider pools are
  indexed: Monad RPC nodes support `debug_traceCall`.

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
