# base-pancakeswap-infinity-bin

Substreams package for PancakeSwap Infinity Liquidity Book (Bin) pools. Emits protocol system
`pancakeswap_infinity_bin` with component type `pancakeswap_infinity_bin_pool`.

Two manifests share one wasm binary, because Infinity is deployed at identical addresses on both
chains and only the start block differs:

| Manifest | Chain | `BinPoolManager` first block |
| --- | --- | --- |
| `base-pancakeswap-infinity-bin.yaml` | Base | 30544163 |
| `bsc-pancakeswap-infinity-bin.yaml` | BNB | 47214336 |

Sibling of `../base-pancakeswap-infinity-cl`, which shares the Vault, the PoolKey, the hook bitmap
and the filtering. Bin differs in the pool math: a pool is a map of discrete bins to
`(reserveX, reserveY)` plus an active bin, not a sqrt price with a tick map. Two consequences:
per-bin reserves are read from `BinPoolManager` storage diffs because no event carries them, and
the attribute schema is defined here rather than borrowed from Uniswap v4, so these components
need `PancakeswapInfinityBinState` and cannot be decoded by `UniswapV4State`.

## Scope

Included: pools with a static LP fee and no swap hook.
Excluded: swap-hook pools, dynamic-fee pools. Unlike the CL package, `Donate` IS indexed, because
in Bin it moves real reserves rather than only fee growth.

`bins/{id}` is emitted only while the bin holds reserves and a swap can reach it: while it is in
the pool's tree, or while it is the active bin, where a swap starts whatever the tree says. A burn
down to `MINIMUM_SHARE` drops a bin from the tree with dust still in `reserveOfBin`, so the
attribute is deleted even though reserves are not zero, unless the bin is still active. A swap
can move off an active dust bin without writing it; `store_bin_reserves` tells whether it held
dust, and so was published and must be deleted.

Not tracked: pausing. The owner can [pause](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/Owner.sol#L10-L19) `BinPoolManager`: [`swap`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/BinPoolManager.sol#L138-L141), [`mint`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/BinPoolManager.sol#L185-L188) and [`donate`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/BinPoolManager.sol#L260-L263) then revert with `EnforcedPause`, while [`burn`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/BinPoolManager.sol#L229) still allows removals. [`Paused`/`Unpaused`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/base/Pausable.sol#L20-L25) fire once on the pool manager, not per pool. Marking every component paused on each event means iterating the whole pool store, which this package does not do. Simulation keeps quoting through a pause and execution reverts.

## Build

```bash
cd protocols/substreams
# From the package dir, so protogen uses its buf.gen.yaml instead of writing one here.
(cd base-pancakeswap-infinity-bin && substreams protogen base-pancakeswap-infinity-bin.yaml --exclude-paths="sf,google,tycho")
cargo build --package base-pancakeswap-infinity-bin --target wasm32-unknown-unknown --release
cargo test --package base-pancakeswap-infinity-bin
cd base-pancakeswap-infinity-bin
substreams run base-pancakeswap-infinity-bin.yaml map_protocol_changes -e base-mainnet.streamingfast.io:443 --start-block 30544163 -t +100
substreams run bsc-pancakeswap-infinity-bin.yaml map_protocol_changes -e bnb.streamingfast.io:443 --start-block 47214336 -t +100
```
