# Changelog

## v0.9.0

- Index the stored LP fee of dynamic-fee pools. A pool whose key fee is `0x800000` charges the
  `lpFee` in `slot0` when its hook returns no override, and `updateDynamicLPFee` sets it without
  an event. The new `store_dynamic_fee_pools` tracks each such pool's `slot0` slot, and
  `map_protocol_changes` (and `map_pons_enriched_block_changes` on Robinhood) emit an `lp_fee`
  attribute whenever a PoolManager write changes it. `map_protocol_changes` now takes the
  PoolManager address as a parameter, and the Robinhood Pons parameters gain `pool_manager`.
- Needs a resync: pools keep an `lp_fee` of 0 until a backfill from the first block replays their
  fee writes.

## v0.8.1

- Add the Robinhood Chain Uniswap V4 with-hooks manifest. Pons V2 MemeHook pools get static
  `hook_identifier`, `pons_hook_fee_bps` and `pons_creator_tax_bps` attributes decoded from the
  hook's storage writes in the creation transaction. The Robinhood module takes its hooks from a
  single parameter, `pons_hooks=<address>[,<address>...]`; its default remains the canonical
  deployment only.
- Move the Ethereum and Unichain manifests to `v0.8.1` so all three manifests declare the version
  of the single binary they are built from.
