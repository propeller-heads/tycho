# Changelog

## v0.9.0

- Add the Elysium manifest `elysium-uniswap-v4-with-hooks.yaml`, which indexes Montra launchpad
  pools under `uniswap_v4_hooks`. Its own creation module, `map_montra_pools_created`, creates
  only pools whose hook is listed in its `montra_hooks` parameter.
- The terminal module `map_montra_enriched_block_changes` decodes the Montra hook's `PoolConfig`
  storage writes into `montra_*` state attributes.
- That module also emits `paused`. A pool is paused until a transaction places its seed, and that
  transaction unpauses it.
- A `PoolKey` word that changes after the pool's creation pauses the pool. Only a seed placed on
  the pool unpauses it.
- The module fails the block on a storage change whose address is not 20 bytes, and on a Montra
  hook storage change whose key or value is longer than 32 bytes.
- Move the Ethereum, Unichain and Robinhood manifests to `v0.9.0` so all four manifests declare
  the version of the single binary they are built from. Their module graphs, parameters and output
  are unchanged.

## v0.8.1

- Add the Robinhood Chain Uniswap V4 with-hooks manifest. Pons V2 MemeHook pools get static
  `hook_identifier`, `pons_hook_fee_bps` and `pons_creator_tax_bps` attributes decoded from the
  hook's storage writes in the creation transaction. The Robinhood module takes its hooks from a
  single parameter, `pons_hooks=<address>[,<address>...]`; its default remains the canonical
  deployment only.
- Move the Ethereum and Unichain manifests to `v0.8.1` so all three manifests declare the version
  of the single binary they are built from.
