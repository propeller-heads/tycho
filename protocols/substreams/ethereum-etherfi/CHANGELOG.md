# Changelog

## v0.1.0

Initial EtherFi integration. Indexes two components from raw storage slots:

- `eETH` (`0x35fA...8ac2`) — ETH deposits through the LiquidityPool and eETH redemptions
  through the RedemptionManager, including the `ethBucketLimiter` redemption rate limit.
- `weETH` (`0xCd5f...b7ee`) — eETH wrap and unwrap.

Both contracts were deployed in 2023, so the package does not pick the components up from their
creation transactions - it would have to index from there to reach today's state.
`map_protocol_components` instead takes the chain state at its `initialBlock` as a `params`
snapshot and emits both components at that block, seeded with the slot values and balances read
there. `scripts/compute_initial_state.sh <block>` regenerates the snapshot for a new start
block.

Balances come from the protocol's own share accounting, not from the components' own token
balances, so the integration test runs with `skip_balance_check`.
