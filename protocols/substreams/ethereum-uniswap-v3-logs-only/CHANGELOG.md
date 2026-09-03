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

## v0.1.3

- Add the Robinhood Chain Uniswap V3 logs-only manifest.
- Remove a redundant reference in a store-key format argument.
