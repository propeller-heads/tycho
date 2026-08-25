# Changelog

## v0.6.0

- Index several factory generations per pool family. The `weighted_factory`,
  `stable_factory`, and `reclamm_factory` deployment parameters are replaced by
  `weighted_factories`, `stable_factories`, and `reclamm_factories`, each a map
  from a version label to a factory address:

  ```
  weighted_factories[v1]=201efd…&weighted_factories[v2]=…
  ```

  Balancer keeps the `create` signature and the `PoolCreated` event stable
  across generations, so one decoder serves every version of a family. The
  version label is free-form and names the generation for whoever reads the
  manifest; only the address is matched. A family may be omitted entirely when
  it is not deployed on a chain.

- Reject a factory address configured under two version labels, and reject an
  address that is not 20 bytes, at parameter-parsing time.
- Fail `map_components` when no factory of any family is configured, since that
  module can only discover pools by matching factories.
- Configure every deployed weighted and stable generation on all four chains,
  verified against the
  [Balancer deployments repo](https://github.com/balancer/balancer-deployments):
  two weighted (`v1`, `v2`) and three stable (`v1`, `v2`, `v3`) per chain. The
  previous configuration named one generation per family, and on Ethereum both
  the weighted and the stable factory were the deprecated December 2024
  deployments, so no pool created by a later factory was indexed.

  `create` and `PoolCreated` are byte-identical across every generation of these
  families, so one decoder covers them all.

  reCLAMM stays pinned to the newest generation. `balancer-maths-rust` carries
  the first reCLAMM generation as a separate implementation from the second, and
  Balancer confirmed only that the second and third share their swap maths — so
  an older reCLAMM pool cannot be priced through this decoder.

  Note that the contract *names* `WeightedPoolFactory` and `StablePoolFactory`
  are also used by Balancer V2 deployments, whose `create` signature differs.
  Only tasks whose id contains `-v3-` belong here.

- Write the factory family alone into the `pool_type` static attribute, for
  example `WeightedPoolFactory`. A family prices the same whichever generation
  built the pool, so the generation label stays in the manifest rather than
  travelling with every component. Where a generation does differ, the consumer
  probes the pool for the feature — the weighted minimum balance is read from
  the pool itself — and reCLAMM's earlier maths is kept out by configuring only
  its newest factory.

- Skip pools created with an external hooks contract. Hooks run arbitrary code
  on swaps, which the native maths in `tycho-simulation` does not model, so
  such pools could never be quoted. This also drops pools whose hook only
  intervenes in liquidity operations, since the hook's flags — which would
  tell the two apart — only exist on-chain. The reCLAMM factory takes no hooks
  parameter, so only the weighted and stable families are filtered.

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
