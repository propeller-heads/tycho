# ABIs

These are hand-written minimal subsets, not full Etherscan/Blockscout dumps: only the
events and functions this package decodes or calls. `build.rs` runs `abigen` over every
`.json` here and writes `src/abi/`.

ERC-20 is not here — `tycho_substreams::abi::erc20` already provides `Transfer`. Multicall3 is
not here either: `substreams_ethereum::rpc::RpcBatch` batches typed calls off these bindings and
reports per-call failure, which is what `aggregate3(allowFailure)` was wanted for.

## Factory generations

Pendle has two incompatible factory ABIs on Ethereum. The take-home brief states all five
factories emit the same `CreateNewMarket`; they do not.

| ABI file | Factories | `CreateNewMarket` | topic0 |
|---|---|---|---|
| `pendle_market_factory_v1.json` | `0x27b1dAcd…0784` (original) | `(market, PT, scalarRoot, initialAnchor)` | `0x166ae5f5…3143` |
| `pendle_market_factory.json` | `0x1A6fCc85…2D52` (V3), `0x3d75Bd20…7A2F` (V4), `0x6fcf753f…D050` (V5), `0x6d247b1c…9A9f` (V6) | `(market, PT, scalarRoot, initialAnchor, lnFeeRateRoot)` | `0xae811fae…20af` |

The original factory carries no `lnFeeRateRoot` in the event and keys its fee override by
**router only** (`getMarketConfig(router)`, `SetOverriddenFee(router, lnFeeRateRoot,
reserveFeePercent)`), while V3+ keys it by `(market, router)`. V3, V4, V5 and V6 all report
`PendleMarketFactoryV3` / `PendleMarketFactoryV7Upg` and share one event set, so they share
one ABI.

## Verification

Every signature below was checked against mainnet, not taken from documentation.

Topic hashes confirmed by decoding real logs (`eth_getLogs` via `https://eth.drpc.org`):

| Event | topic0 | Where confirmed |
|---|---|---|
| `CreateNewMarket` (V3+) | `0xae811fae…20af` | 7 logs on factory V6, blocks 25.72M–25.81M |
| `CreateNewMarket` (v1) | `0x166ae5f5…3143` | wstETH market creation, tx `0xf5022404…5daa`, block 17363218 |
| `SetOverriddenFee` (V3+) | `0xea7fdf3a…e59f` | 89 logs on factory V6 |
| `Swap` | `0x829000a5…57c4` | event replay, `scripts/verify_event_replay.py` |
| `Mint` | `0xb4c03061…accb` | same |
| `Burn` | `0x4cf25bc1…5f90` | same |
| `UpdateImpliedRate` | `0x5c0e21d5…f83e1` | same |
| `NewInterestIndex` | `0x71475f2f…703d` | YT `0x04b7fa1e…3a95`, block 25807775; `topics[1]` equals `pyIndexStored()` at the same block |

Parameter types and `indexed` flags come from the verified sources on Blockscout:
`PendleMarketFactory` `0x27b1dAcd…0784`, `PendleMarketFactoryV3` `0x1A6fCc85…2D52`,
`PendleMarketFactoryV7Upg` `0xe7a7477c…6c0c` (the V6 proxy's implementation),
`PendleMarketV7` `0x47ad2cd1…f77e`, `PendleYieldToken` `0x89e6e5f7…ce44`,
`PendleWstEthSY` `0xcbc72d92…c0bc`.

Two of those flags are easy to get wrong and are load-bearing:

- `NewInterestIndex(uint256 indexed newIndex)` — the index is in **topics[1]**, not in the
  data. Both YT generations (`PendleYieldToken` on the original wstETH market and on a V6
  market) declare it indexed.
- `UpdateImpliedRate(uint256 indexed timestamp, uint256 lnLastImpliedRate)` — the timestamp
  is indexed, the rate is not.

Function selectors were confirmed by calling them on mainnet: `readTokens()` `0x2c8ce6bc`,
`expiry()` `0xe184c9be`, `factory()` `0xc45a0155` (all three on the wstETH market
`0x34280882…be3b`, returning the values the brief quotes), `pyIndexStored()` `0xd2a3584e`
and `pyIndexCurrent()` `0x1d52edc4` (on YT `0x89e6e5f7…ce44`), and `assetInfo()`
`0xa40bee50`, `exchangeRate()` `0x3ba0b9a9`, `getTokensIn()` `0x213cae63`,
`getTokensOut()` `0x071bc3c9`, `previewDeposit(address,uint256)` `0xb8f82b26` (the SY
sweep, `scripts/sweep_sy_classes.py`).

`pyIndexCurrent()` is declared `nonpayable` because it is: it writes the cached index. It
is only ever `eth_call`ed here, never sent.
