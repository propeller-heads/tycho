# base-pancakeswap-infinity-cl

Substreams package for PancakeSwap Infinity concentrated-liquidity pools. Emits protocol system `pancakeswap_infinity_cl` with component type `pancakeswap_infinity_cl_pool`.

Three manifests share one wasm binary. Base and BNB use the same CREATE3 addresses; Robinhood is a separate deployment, so the addresses are manifest params:

| Manifest | Chain | `CLPoolManager` | `Vault` | First block |
| --- | --- | --- | --- | --- |
| `base-pancakeswap-infinity-cl.yaml` | Base | `0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b` | `0x238a358808379702088667322f80aC48bAd5e6c4` | 30544106 |
| `bsc-pancakeswap-infinity-cl.yaml` | BNB | `0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b` | `0x238a358808379702088667322f80aC48bAd5e6c4` | 47214308 |
| `robinhood-pancakeswap-infinity-cl.yaml` | Robinhood | `0xeE04c68742e6Bf434bE8039580D2e89BBE55bc6f` | `0x4F922d5B15e6691e0469663E4F5C4177f23c5FaF` | 56743018 |

Infinity CL is a Uniswap v4 fork, so this package is a port of `../ethereum-uniswap-v4` (`shared/` + `no-hooks/`) and emits the same attribute schema; `UniswapV4State` simulates the components unchanged. The protocol differences and where they land are described in `src/lib.rs`.

## Scope

Included: pools with a static LP fee and either no hook or a hook without swap permissions (bits 6, 7, 10, 11 of `PoolKey.parameters`). Excluded: swap-hook pools, dynamic-fee pools. `Donate` is ignored for balances, as in v4.

Not tracked: pausing. The owner can [pause](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/Owner.sol#L10-L19) `CLPoolManager`: [`swap`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/CLPoolManager.sol#L162-L165) and [`donate`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/CLPoolManager.sol#L215-L218) then revert with `EnforcedPause`, and [`modifyLiquidity`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/CLPoolManager.sol#L131-L132) rejects adds but still allows removals. [`Paused`/`Unpaused`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/base/Pausable.sol#L20-L25) fire once on the pool manager, not per pool. Marking every component paused on each event means iterating the whole pool store, which this package does not do. Simulation keeps quoting through a pause and execution reverts.

## Build

```bash
cd protocols/substreams
# From the package dir, so protogen uses its buf.gen.yaml instead of writing one here.
(cd base-pancakeswap-infinity-cl && substreams protogen base-pancakeswap-infinity-cl.yaml --exclude-paths="sf,google,tycho")
cargo build --package base-pancakeswap-infinity-cl --target wasm32-unknown-unknown --release
cargo test --package base-pancakeswap-infinity-cl
cd base-pancakeswap-infinity-cl
substreams run base-pancakeswap-infinity-cl.yaml map_protocol_changes -e base-mainnet.streamingfast.io:443 --start-block 30544106 -t +100
substreams run bsc-pancakeswap-infinity-cl.yaml map_protocol_changes -e bnb.streamingfast.io:443 --start-block 47214308 -t +100
substreams run robinhood-pancakeswap-infinity-cl.yaml map_protocol_changes -e mainnet.robinhood.streamingfast.io:443 --start-block 56743018 -t +100
```
