# Changelog

## v0.2.0

- Index the plugin's MEVX settings per pool as the `mevx_router`, `min_gas_left` and
  `call_gas_budget` attributes, from the plugin's `MevxRouterSet`, `MinGasLeftSet` and
  `CallGasBudgetSet` events. The new `map_plugin_events` module attributes a plugin's logs to its
  pool through a reverse entry in `store_pool_plugins`.
- Index the plugin's farming `incentive` from its `Incentive` event. When a pool switches plugin,
  the plugin-scoped attributes reset so nothing carries over from the detached plugin, whose
  later events no longer reach the pool.
- Re-register the plugin's `beforeSwap` entrypoint on every swap of a pool that enables it, so a
  plugin whose trace failed before trading started is traced again once a swap shows it works.

## v0.1.0

- Initial release: Nami V4 (Algebra Integral 1.2 fork) indexing on Ethereum (factory
  `0xD1Bf4A24DC17Da21410a7b047471E191eA538d43`), Base (factory
  `0xf4502A98A15e82D66d28D2381E2F3A407ea45b83`) and Robinhood Chain (factory
  `0xDeDB80903df19f9dB9b393eb66E07511CB9ABdA2`), one manifest per chain.
- Ticks are created and deleted on their gross liquidity, not their net liquidity: an Algebra
  tick bounds swap steps while any position references it, even when its net liquidity is zero.
- When a pool enables its plugin's `beforeSwap`, the plugin's `beforeSwap` is registered with the
  Dynamic Contract Indexer (called by the pool), so the plugin's and the pool's code and storage
  are indexed for fee simulation. Block storage changes are emitted for that purpose.
