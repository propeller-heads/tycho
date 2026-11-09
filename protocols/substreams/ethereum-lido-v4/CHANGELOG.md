# Changelog

## v0.1.0

Lido integration for the core v4.0.0 storage layout, supported from block 25603297.

One component (`0xae7a...fE84`, stETH) serves four directions using a shared pool state:

- `ETH -> stETH` through `Lido.submit`.
- `ETH -> wstETH` through wstETH's payable `receive()`.
- `stETH -> wstETH` through `wstETH.wrap`.
- `wstETH -> stETH` through `wstETH.unwrap`.

Unstaking requires the asynchronous withdrawal queue, so both token-to-ETH directions report
zero limits. Deposits respect staking capacity and available uint128 storage capacity. Wraps
are bounded by stETH supply and unwraps by the wrapper's shares. Unwrap quotes use the value of
the shares actually transferred; amounts rounding to zero output are rejected.

The manifest contains an end-of-block snapshot and an anchor transaction for component
creation. `scripts/compute_initial_state.sh` regenerates the snapshot and verifies its fields
against the contract getters using an archive RPC. Successful storage writes are processed in
execution ordinal order, preserving the final state of nested calls.

The component reports `getTotalPooledEther()` as its ETH balance: buffered ether, consensus-layer
validator and pending balances, deposits since the last report, and the ether backing external
shares. The balance store carries these inputs across blocks. Integration tests use
`skip_balance_check` because this accounting balance includes ETH outside the stETH contract.

The manifest records the verified stETH implementation. The package watches the Aragon Kernel's
`SetApp` events and pauses the component when a transaction ends on another implementation.
State updates continue while paused. Resuming requires verification of the storage layout and
swap behavior against the new implementation. Ordinary protocol-wide pauses are not indexed.
