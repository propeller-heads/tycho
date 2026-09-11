# Changelog

## v0.1.0

Initial Lido V3 integration. Indexes two components from raw storage slots:

- `stETH` (`0xae7a...fE84`) — ETH staking. One-directional: unstaking runs through the
  asynchronous withdrawal queue, so there is no stETH -> ETH quote.
- `wstETH` (`0x7f39...2Ca0`) — stETH wrap and unwrap.

Both contracts predate the package, so the module graph does not discover them from a
creation event. Instead the manifest carries a state snapshot in `params` and the
components are created at `start_block`; regenerate the snapshot for a different start
block with `scripts/compute_initial_state.sh`.

Component balances are reported as absolute values on every transaction that moves one of the
inputs:

- the stETH component reports `getTotalPooledEther()` in ETH, derived from the tracked
  `buffered_ether`, `cl_balance`, `deposited_validators` and `cl_validators` halves;
- the wstETH component reports the stETH locked in the wrapper
  (`sharesOf(wstETH) * totalPooledEther / totalShares`, i.e. `stETH.balanceOf(wstETH)`), which
  is its tradable liquidity. Reporting the pool total here would overstate it ~2.3x and
  double-count the protocol's TVL.

Carrying those inputs across blocks needs a `store_balance_slots` store module: only two of the
tracked slots feed `totalPooledEther`, and a block that touches one usually leaves the other
untouched.

The integration test keeps `skip_balance_check`: the stETH component's balance is protocol
accounting, not the stETH contract's own ETH balance (which only holds the buffered ether).
