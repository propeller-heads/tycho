# Changelog

## v0.10.0

- Index Ascend launchpad pools on Elysium in `elysium-uniswap-v4-with-hooks.yaml`, next to the
  Montra pools. The new parameter `ascend_hooks` (`map_montra_pools_created`,
  `store_montra_slots`, `map_montra_enriched_block_changes`) lists the Ascend hooks; it is
  required, refuses the zero address, and may not share a hook with `montra_hooks`.
- `map_montra_pools_created` creates a pool for each `Initialize` of a listed Montra or Ascend
  hook.
- `store_montra_slots` takes the params and keeps five keys per Ascend pool (`PoolData` words 2
  and 3, `Bid` words 0 to 2), valued with word indexes 7 to 11.
- `map_montra_enriched_block_changes` decodes those words into the `ascend_*` state attributes
  (fee, orientation, start tick, EMA, observation time, observed tick, discount, bid inventory,
  filled in window, window start, max clip), created with the pool and updated by every write to
  them. Ascend pools are never paused: `setPaused` stops launches only.
- The Elysium manifest's `initialBlock` moves from 588510 to 423647, the Ascend hook's deployment
  block, on every module. Its output for the Montra pools is unchanged, but the manifest has not
  been released, so nothing resyncs.
- Move the Ethereum, Unichain, Robinhood and Elysium manifests to `v0.10.0`, the version of the
  single binary. Their other module graphs, parameters and output are unchanged; the binary is
  new, so their module hashes change.

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
- The module fails the block on a Montra hook storage change whose key or value is longer than 32
  bytes. A storage change whose address is not 20 bytes is skipped.
- `store_montra_slots` keys each slot by the pool's hook and the slot, so a write is attributed
  to a pool only when the pool's own hook made it.
- Address-list parameters (`montra_hooks`, and Pons' `pons_hooks`) are parsed by one shared parser
  that also refuses the zero address. Pons' error texts change with it: "entry N `x` is not
  exactly 20 bytes" becomes "value `x` is N bytes, not 20", and "duplicates an earlier address"
  becomes "lists `x` twice".
- `map_pools_created` parses its `pool_manager` parameter once, with a named error instead of a
  panic, and accepts it with or without `0x`.
- Move the Ethereum, Unichain and Robinhood manifests to `v0.9.0` so all four manifests declare
  the version of the single binary they are built from. Their module graphs, parameters and output
  are unchanged, but the binary is new, so their module hashes change: an extractor moved to
  `v0.9.0` takes the new hash, with no change in what it emits.

## v0.8.1

- Add the Robinhood Chain Uniswap V4 with-hooks manifest. Pons V2 MemeHook pools get static
  `hook_identifier`, `pons_hook_fee_bps` and `pons_creator_tax_bps` attributes decoded from the
  hook's storage writes in the creation transaction. The Robinhood module takes its hooks from a
  single parameter, `pons_hooks=<address>[,<address>...]`; its default remains the canonical
  deployment only.
- Move the Ethereum and Unichain manifests to `v0.8.1` so all three manifests declare the version
  of the single binary they are built from.
