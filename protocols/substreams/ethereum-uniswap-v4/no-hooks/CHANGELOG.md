# Changelog

## v0.4.3

- Index only pools with no hook and a static LP fee (all manifests bumped to v0.4.3). The filter used to admit any hook without
  swap permission bits; such a hook can still call `updateDynamicLPFee` on a dynamic-fee pool and
  change the fee between the indexed state and a swap.

## v0.4.2

- Add the Robinhood Chain Uniswap V4 no-hooks manifest.
