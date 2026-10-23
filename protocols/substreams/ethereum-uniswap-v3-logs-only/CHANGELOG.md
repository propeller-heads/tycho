# Changelog

## v0.1.4

- Take `protocol_type_name` as a `map_pools_created` parameter instead of hardcoding
  `uniswap_v3_pool`, so a fork indexed by this package emits its own protocol type. Every manifest
  now passes its parameters as a query string.
- Fix the declared `map_pools_created` output type in the Arbitrum, BSC, and Robinhood manifests.
  They said `BlockChanges` while the module returns `BlockEntityChanges`, so `substreams run` and
  `substreams gui` could not decode that module's output. The three manifests also omitted
  `tycho/evm/v1/entity.proto`, which is why they could not name the right type; it is now imported.
  The wire data was always `BlockEntityChanges` — only the declaration was wrong, and
  `map_protocol_changes`, the module the indexer consumes, was unaffected.
- Add the Robinhood Chain SushiSwap V3 manifest, `robinhood-sushiswap-v3.yaml` (factory
  `0xe51960f1b45f1c9fb6d166e6a884f866fc70433b`, first pool at block `6606213`). The deployment is
  canonical Uniswap V3: same factory and pool event layouts, immutable per-pool fee, and the
  `uniswapV3SwapCallback` family. The wasm is unchanged, so this manifest ships on the v0.1.4 build
  and needs its `protocol_type_name` parameter; components are emitted as `sushiswap_v3_pool`.
- Add the Robinhood Chain RobinSwap V3 manifest, `robinhood-robinswap-v3.yaml` (factory
  `0xea561e058313b96011e5070ca7d0f027a44e3748`, first pool at block `6066330`). The deployment uses
  canonical Uniswap V3 events and swap mathematics. Pools initialize `feeProtocol` to
  `7 + (7 << 4)`, which splits the configured swap fee differently between LPs and the protocol but
  leaves the trader's amount out and the event-derived balances unchanged. The wasm is unchanged,
  so this manifest ships on the v0.1.4 build and needs its `protocol_type_name` parameter;
  components are emitted as `robinswap_v3_pool`.
- Add the Ethereum CommunityV3 manifest, `ethereum-communityv3.yaml` (factory
  `0x00a38533c68327aBE0Cc265436e036066E2D6B38`, deployed at block `25895766`). The deployment is
  canonical Uniswap V3: same contracts, pool init code hash, factory and pool event layouts,
  immutable per-pool fee, and the `uniswapV3SwapCallback` family. Its factory owner is burned, so
  `feeProtocol` stays 0. The wasm is unchanged, so this manifest ships on the v0.1.4 build and
  needs its `protocol_type_name` parameter; components are emitted as `communityv3_pool`.


## v0.1.3

- Add the Robinhood Chain Uniswap V3 logs-only manifest.
- Remove a redundant reference in a store-key format argument.
