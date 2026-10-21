# Changelog

## v0.1.0

Initial EtherFi integration, against the implementations live since the escrow migration at
block 25533308. Two components, both driven by raw storage writes:

- `eETH` (`0x35fA...8ac2`) — ETH deposits through `LiquidityPool.deposit` and eETH redemptions
  through `EtherFiRedemptionManager.redeemEEth`. Carries the pool totals, the redemption
  manager's fee and watermark parameters and rate-limit bucket, and the eETH mint and burn
  buckets on `EtherFiRateLimiter` that bound deposits and redemptions.
- `weETH` (`0xCd5f...b7ee`) — eETH wrap and unwrap. Carries the pool totals and
  `eETH.shares(weETH)`, which bounds unwrapping.

The manifest records the implementation behind each of the five proxies the storage and simulation
were verified against, including weETH. All five are EIP-1967, so the package watches their
implementation slot and pauses both components on the block that installs any other implementation.
They stay paused until the slots and swap behavior are re-verified and a new snapshot is taken.

Both contracts were deployed in 2023, so the package does not pick the components up from their
creation transactions - it would have to index from there to reach today's state. The manifest
instead carries the tracked slot values at `start_block` in `params` and anchors the components
to a transaction in that block. `scripts/compute_initial_state.sh <block>` regenerates the
snapshot for a new start block.

Component balances are derived from the tracked slots on every block that moves one of them:
`totalValueInLp` for the pool, and the wrapper's shares at the current share rate for the
wrapper, so a rebase moves the wrapper balance without any transfer. They are the protocol's own
accounting rather than token balances of the component addresses, so the integration test runs
with `skip_balance_check`.
