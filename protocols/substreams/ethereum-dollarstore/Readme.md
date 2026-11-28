# ethereum-dollarstore

Indexes DollarStore, a singleton that swaps listed stablecoins 1:1 in normalized 6-decimal units,
with no fee, as the VM protocol `vm:dollarstore`.

## Components

Assets sit in the hub pool (pool 0) or in spoke pools (one asset each). The store swaps hub-hub
and hub-spoke pairs in one call (`swapExactInput`); spoke-spoke needs two legs and is not a
component. A component is one pair:

- id: `keccak256(store ++ token0 ++ token1)` with `token0 < token1`. The adapter checks every call's
  tokens against it.
- tokens: the two assets, sorted.
- contracts: the store proxy.
- created by the `AssetListed` event of the pair's second asset. An asset listed again (after its
  spoke pool is removed) creates nothing new.

## What is indexed

| Data | Source |
|---|---|
| Proxy storage (reserves, queues, asset configs, pause flags, peg settings) | storage diffs of the proxy, from its creation block |
| Implementation, the swap library it delegatecalls | DCI: `getSwapQuote(offer, want, max)` on the proxy, in each direction, per component |
| Price feeds | DCI: `latestRoundData()` and `decimals()` on the asset's current feed, linked to every pair of the asset when a pair is created and on each `AssetListed` or `PriceFeedUpdated` |
| Proxy token balances (the VM's `balanceOf` for the store) | ERC-20 `Transfer`s to and from the proxy, every token, from the proxy's creation; emitted for listed tokens |
| Component balances | the proxy's whole balance of each of the pair's tokens |

The feed entrypoints exist because `getSwapQuote` stops before reading the feed while the store, the
pool or the asset's deposits are paused, and DCI retraces only when an address it called changes, not
when a pause flag clears.

## Tradability

The package emits no pause attribute. Every condition that stops a swap is store state the VM
reads, and `getSwapQuote` returns 0 for it, so the adapter's limits are zero:

- global pause, pool pause, deposit pause of the sell token;
- a removed spoke pool (its asset is delisted);
- a winding-down spoke (spoke-to-hub only);
- a missing, stale or off-peg price feed for the sell token (`maxStaleness` against the block
  timestamp, `pegTolerance`).

## Upgrade boundary

| Dependency | How the package follows it |
|---|---|
| Implementation behind the proxy | DCI: the proxy delegatecalls the address in its EIP-1967 slot, so an upgrade retraces |
| Swap library linked into the implementation | DCI, reached from the implementation when the store is not paused at trace time |
| Price feed per asset (`setPriceFeed`, or listing the asset again) | `PriceFeedUpdated` or the repeated `AssetListed` adds entrypoints on the new feed |
| Event `AssetListed` | component discovery; a new implementation that lists assets differently needs a package change |
| Adapter ABI: `getSwapQuote`, `swapExactInput`, `assetScalingFactor`, `assetPoolId`, `poolCount`, `getPoolAssets`, `isAssetListed` | compiled into the adapter; a change to them needs a new adapter |

## Known limitations

- In the VM, tycho-simulation replaces token code with its token proxy, so a payout to a queued
  counterparty that the token freezes succeeds in simulation. On chain the store parks that position,
  fills less, and `swapExactInput` reverts.
- Pairs that pay out the same asset draw on one reserve, and each reports the whole reserve as its
  limit. A swap simulated on one pair does not lower the other's limit, so an order split across
  both can exceed the reserve and revert on chain.
- An asset listed first as a spoke and later relisted as a hub gets no new pairs with the other
  spokes.
- A removed spoke's asset stays in the package's asset list, so a hub asset listed later gets a pair
  with it. That pair quotes 0 until the asset is relisted, and then trades with no new component.
- The swap library is reached only through `getSwapQuote`, which returns 0 before calling it while
  the store is paused. If an upgrade lands while the store is paused, the new library is not indexed
  and quotes fail after unpausing until the `getSwapQuote` entrypoints are retraced. DollarStore does
  not upgrade while paused; if it ever does, it asks the Tycho team to retrace them.
- The integration test range ends at 26299700, where the hub holds no USDS and the spoke cannot pay
  out more hub assets, so those directions quote 0. The adapter fork tests cover every direction
  at block 26424000.
