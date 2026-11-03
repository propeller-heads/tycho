# Changelog

## v0.1.0

- Index AERO v1.1.0 concentrated-liquidity pools (protocol system `aero_cl`, type
  `aero_cl_pool`) on Base, Arc, Ethereum, Arbitrum and Robinhood. Based on
  `base-aerodrome-slipstreams`, with the v1.1 pool layout: liquidity at slot 13, ticks at 14,
  observations from 17.
- Fee inputs come from storage writes of the factory and the `DynamicSwapFeeHook`:
  - per pool: `dfc_*` (the hook's packed `dynamicFeeConfig`), `pool_swap_hook` (the factory's
    per-pool hook override), and `block_fee` / `block_fee_block` (the fee the hook cached for the
    pool's last traded block);
  - shared, copied into each pool at creation and fanned out on change: `default_swap_hook`,
    `hook_default_scaling_factor`, `hook_default_fee_cap`, `hook_seconds_ago`.
