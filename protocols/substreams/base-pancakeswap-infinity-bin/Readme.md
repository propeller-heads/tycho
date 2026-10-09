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

`bins/{id}` is emitted only while the bin is in the pool's tree, which is what the swap loop can
reach. A burn down to `MINIMUM_SHARE` drops a bin from the tree with dust still in `reserveOfBin`,
so the attribute is deleted even though reserves are not zero. One gap: a dust bin that is still
the active bin is traded through on chain but hidden here.

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
