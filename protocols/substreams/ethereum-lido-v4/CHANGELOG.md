# Changelog

## v0.1.0

Initial Lido integration, on the storage layout Lido core v4.0.0 introduced at block 25603297.

One component (`0xae7a...fE84`, the stETH contract) covers the whole venue and serves four
directions:

- `ETH -> stETH` — staking through `Lido.submit`.
- `stETH <-> wstETH` — wrap and unwrap.
- `ETH -> wstETH` — wstETH's `receive()` stakes and wraps in one call, which saves the hop
  through stETH.

Unstaking runs through the asynchronous withdrawal queue, so `stETH -> ETH` and `wstETH -> ETH`
report a zero limit. Keeping the venue in one component is what lets `ETH -> stETH` exist exactly
once: split across two components, the one that cannot perform it would advertise it anyway.

The contracts predate the package, so the module graph does not discover them from a creation
event. The manifest carries a state snapshot in `params` and the component is created at
`start_block`; regenerate the snapshot for a different start block with
`scripts/compute_initial_state.sh`. The snapshot has to be taken at or after block 25603297:
Lido v4 (Staking Router v3, LIP-35) moved the pooled-ether accounting from validator counts to
balances and zeroed the slots the previous layout used.

The component reports one absolute balance, `getTotalPooledEther()` in ETH: `bufferedEther +
clValidatorsBalance + clPendingBalance + depositedPostReport`, plus the ether backing the
external (stVaults) shares at the same share rate. The stETH the wrapper holds is already inside
that figure, so reporting it as well would count the same ether twice.

Carrying those inputs across blocks needs a `store_balance_slots` store module: a block that
touches one of the tracked slots usually leaves the others untouched.

The integration test keeps `skip_balance_check`: the stETH component's balance is protocol
accounting, not the stETH contract's own ETH balance (which only holds the buffered ether).
