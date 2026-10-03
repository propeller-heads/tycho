# base-pancakeswap-infinity-cl

Substreams package for PancakeSwap Infinity concentrated-liquidity pools. Emits protocol system
`pancakeswap_infinity_cl` with component type `pancakeswap_infinity_cl_pool`.

Two manifests share one wasm binary, because Infinity is deployed at identical addresses on both
chains and only the start block differs:

| Manifest | Chain | `CLPoolManager` first block |
| --- | --- | --- |
| `base-pancakeswap-infinity-cl.yaml` | Base | 30544106 |
| `bsc-pancakeswap-infinity-cl.yaml` | BNB | 47214308 |

Infinity CL is a Uniswap v4 fork, so this package is a port of `../ethereum-uniswap-v4`
(`shared/` + `no-hooks/`) and emits the same attribute schema; `UniswapV4State` simulates the
components unchanged. The protocol differences and where they land are described in `src/lib.rs`.

## Scope

Included: pools with a static LP fee and either no hook or a hook without swap permissions (bits
6, 7, 10, 11 of `PoolKey.parameters`).
Excluded: swap-hook pools, dynamic-fee pools. `Donate` is ignored for balances, as in v4.

## Build

```bash
cd protocols/substreams
substreams protogen base-pancakeswap-infinity-cl/base-pancakeswap-infinity-cl.yaml --exclude-paths="google"
cargo build --package base-pancakeswap-infinity-cl --target wasm32-unknown-unknown --release
cargo test --package base-pancakeswap-infinity-cl
cd base-pancakeswap-infinity-cl
substreams run base-pancakeswap-infinity-cl.yaml map_protocol_changes -e base-mainnet.streamingfast.io:443 --start-block 30544106 -t +100
substreams run bsc-pancakeswap-infinity-cl.yaml map_protocol_changes -e bnb.streamingfast.io:443 --start-block 47214308 -t +100
```
