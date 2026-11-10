# Changelog

## v0.1.0

- Initial release: Nami V4 (Algebra Integral 1.2 fork) indexing on Ethereum (factory
  `0xD1Bf4A24DC17Da21410a7b047471E191eA538d43`), Base (factory
  `0xf4502A98A15e82D66d28D2381E2F3A407ea45b83`) and Robinhood Chain (factory
  `0xDeDB80903df19f9dB9b393eb66E07511CB9ABdA2`), one manifest per chain.
- Pool balances come from the `Transfer` logs that move a pool's own tokens, so the community
  fee the pool pays its vault without an event of its own is counted.
- Ticks are created and deleted on their gross liquidity, not their net liquidity: an Algebra
  tick bounds swap steps while any position references it, even when its net liquidity is zero.
- When a pool enables its plugin's `beforeSwap`, the plugin's `beforeSwap` is registered with the
  Dynamic Contract Indexer as the pool calls it, and again on every swap, so the plugin's and the
  pool's code and storage are indexed for fee simulation. Block storage changes are emitted for
  that purpose.
- The plugin's `MevxRouterSet`, `MinGasLeftSet`, `CallGasBudgetSet` and `Incentive` events become
  the pool's `mevx_router`, `min_gas_left`, `call_gas_budget` and `incentive` attributes, through
  a reverse entry in `store_pool_plugins`. When a pool switches plugin, the plugin-scoped
  attributes reset, except for the ones the new plugin set in the same block.
