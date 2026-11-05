# bsc-pancakeswap-infinity-cl-hooks

Substreams package for PancakeSwap Infinity concentrated-liquidity pools whose hook is a
`CLAlphaHook`, the hook Binance Alpha listings launch their pools with. Emits protocol system
`pancakeswap_infinity_cl_hooks` with component type `pancakeswap_infinity_cl_pool`. BSC only.

`../base-pancakeswap-infinity-cl` skips every pool with a swap hook. This package indexes the
`CLAlphaHook` ones with that package's modules and attribute schema; it only swaps pool creation
for its own and adds the hook's start time.

## What the hook does

`CLAlphaHook` registers `beforeSwap` and no other swap callback. Its `_beforeSwap` reverts
`PoolNotStarted` unless `poolStartedTimestamp[id] > 0 && poolStartedTimestamp[id] <=
block.timestamp`, and otherwise returns a zero delta and no fee override on a static-fee pool. Once
a pool is open, a swap is exactly a hookless swap.

The owner sets the start time before the pool is initialized and can move it until the pool opens.
The first version (`0x9a9b…`, `0x72e0…`) lets the owner move it after launch too, which pauses the
pool; on BSC that has happened six times.

## Attributes

The CL package's, with two differences:

- static `hooks` is the real hook address (the CL package writes zero), so `UniswapV4State`
  attaches the native `CLAlphaHook` handler registered for these addresses on BSC;
- state `alpha_start_timestamp` is `poolStartedTimestamp[id]` in seconds, zero for a disabled
  pool. It is read from the hook's `PoolStartedAtUpdated` event: at pool creation from
  `store_alpha_start_times`, as of the `Initialize` log, and after that as an update on every event.

## Hooks

The hook addresses are manifest params. A pool is indexed when its hook is listed, its
`parameters` register `beforeSwap` and none of `afterSwap`, `beforeSwapReturnsDelta`,
`afterSwapReturnsDelta`, and its LP fee is static.

| Address | Version |
| --- | --- |
| `0x9a9b5331ce8d74b2b721291d57de696e878353fd` | v1 |
| `0x72e09ebd9b24f47730b651889a4ed984cba53d90` | v1 (same bytecode) |
| `0xb0baa371b899950b4ef6a27c21baf5ef7c434d0f` | v2 |
| `0xb0bb171d333569cfd28a37f5c5dddaaa90ad46af` | v3 |

A new deployment needs both its address here and a handler registration in
`tycho-simulation` (`uniswap_v4/hooks/pancakeswap_clalpha`).

## Build

```bash
cd protocols/substreams
cargo build --package bsc-pancakeswap-infinity-cl-hooks --target wasm32-unknown-unknown --release
cargo test --package bsc-pancakeswap-infinity-cl-hooks
cd bsc-pancakeswap-infinity-cl-hooks
substreams run bsc-pancakeswap-infinity-cl-hooks.yaml map_protocol_changes -e bnb.streamingfast.io:443 --start-block 58349512 -t +100
```
