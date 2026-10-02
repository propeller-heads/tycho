# Tessera on Base — VM integration

Indexes Tessera V (Wintermute's propAMM) on Base as `vm:tessera`. Each Pair contract is one
component; quotes come from executing Tessera's own bytecode in the Tycho VM.

## Data flow

1. Replay Base from block **37,518,600**. A Pair is discovered when its EIP-1967
   implementation, base/quote tokens and Engine registration are written in the same
   committed transaction. Nested-call writes are folded by execution ordinal.
2. The component ID is the Pair address; tokens are sorted. TesseraSwap, the Engine and the
   Pair are stateful contracts, and their raw storage and code changes are indexed.
3. The implementation, pricing library and write helper are published as UTF-8 address
   attributes `stateless_contract_addr_0/1/2` and follow their slot writes. The VM reloads
   bytecode when one changes; a failed load fails the transition.
4. `balance_owner` is TesseraSwap slot 1 (the treasury). A new (token, component) is seeded
   with the treasury's closing `balanceOf`, then follows ERC20 transfers and canonical WETH
   wrap/unwrap events. On a treasury rotation, the block's events are counted against the
   previous treasury and `new_owner_balance - old_owner_balance` is bridged at the rotation.
   A failed `balanceOf` call aborts the block.
5. The adapter quotes with `tesseraSwapViewAmounts`, settles with `tesseraSwapWithAllowances`
   and reports the settled amount. The VM keeps the swap's storage writes for later fills.
6. The Base-only encoder emits the two token addresses (40 bytes). The executor calls
   TesseraSwap with **empty swapData** (fee tag 0) and the router's debit/allowance flow.

## Storage-change map

`map_storage_changes` runs after `store_pairs` and emits, per successful transaction, the
committed writes owned by TesseraSwap, the Engine or a known Pair (including pairs created in
the same block), plus the tag-0 fee slot at any address. Writes are sorted by execution
ordinal. `store_treasury` and `store_safety` read only this map; `map_relative_balances` and
`map_protocol_changes` read it for storage writes and the block for logs and raw contract
changes.

## Upgrade boundary

| Dependency | How it is followed | Assumption |
| --- | --- | --- |
| TesseraSwap | Fixed address, raw state/code | Slot 0 is Engine, slot 1 is treasury, swap/view ABI |
| Engine | Fixed address, raw state/code | Sorted-token registry mapping at slot 8, registration in the discovery transaction |
| Pair implementation | EIP-1967 slot → attribute 0 → bytecode reload | Pair proxy template, token slots 48/49 |
| Pricing library | Pair slot 51 → attribute 1 → bytecode reload | Staticcalled by the implementation; reads no storage |
| Write helper | Pair slot 52 → attribute 2 → bytecode reload | Tag-0 fee is zero |
| Treasury | TesseraSwap slot 1 → `balance_owner` and balance bridge | Standard ERC20 and canonical WETH events |
| Adapter | Pair `poolState()` and token getters | Static ABI; the 20-order ladder only bounds the limit search |

A component is marked `paused` when its write helper's tag-0 fee is nonzero (the VM loads the
helper's code but not its storage, so it would read that fee as zero), and every component is
paused once TesseraSwap points to a different Engine. The safety store records the tag-0 fee
slot at every address from genesis, so a helper that was configured before assignment is
still caught. `paused` is never cleared. Consumers do not act on `paused` yet.

## Limits and price

`getLimits` bounds a search with the ladder sum, seeds it with a small exact-output quote and
runs two exact-input bisection probes, keeping each call under 3M gas. The result is a
conservative hint: up to 25% of the initial interval is left unresolved, so remaining
liquidity can be underestimated. `HardLimits` is not advertised.

`price` is a finite difference of view quotes, with an input step large enough to buy at
least 1,000 raw output units.

Quotes depend on the transaction's priority fee. TesseraSwap prices 4 bps lower, at any size
and in both directions, once `tx.gasprice` exceeds `block.basefee` by more than an
operator-set threshold (0.002 gwei at block 50,548,423, 0.01 gwei at 51,977,873). The Tycho VM
simulates with zero gas price and base fee, so quotes and limits are the zero-priority-fee
price; a fill that pays a priority fee above the threshold receives 4 bps less.

## Tests

- Substreams: unit tests for discovery, the storage-change filter, balance deltas (seed
  suppression, self-transfers, rotations), safety-store writes, dependency attributes and
  pause conditions.
- Adapter: Base fork at block 50,548,423 — both directions, exact in and out, limits, price,
  staleness, two consecutive fills, and settled-output accounting against a synthetic venue.
- Execution: encoder units, executor fork tests, and full Rust-generated router calldata in
  both directions.
- Simulation: VM delta tests for dependency reload and treasury rotation, snapshot decoding,
  and offline consumer tests on `vm/assets/tessera_50548423.json` (returned state and
  indexed-block freshness).

The offline fixture holds public Base bytecode and storage with two expected USDC→WETH view
outputs. It was captured with `debug_traceCall`'s prestate tracer plus `eth_getStorageAt` for
Pair slots 0–52 and TesseraSwap slots 0–2; the second quote overrides Pair slot 3 by +100 USDC.

## Reproduce

From `protocols/substreams`:

```sh
cargo test -p base-tessera
cargo clippy -p base-tessera --all-targets -- -D warnings
cargo build -p base-tessera --release --target wasm32-unknown-unknown
```

From `protocols/adapter-integration/evm` with `BASE_RPC_URL` exported:

```sh
forge test --match-contract TesseraSwapAdapterTest -vv
bash scripts/buildRuntime.sh -c TesseraSwapAdapter -s 'constructor(address)' \
  -a 0x55555522005BcAE1c2424D474BfD5ed477749E3e
```

After adapter changes, copy `out/TesseraSwapAdapter.sol/TesseraSwapAdapter.evm.runtime` to
the simulation VM assets. From the repository root:

```sh
cargo test -p tycho-simulation --lib test_tessera
cargo test -p tycho-simulation --lib evm::protocol::vm::state::tests::test_delta_transition
cargo test -p tycho-execution --lib tessera
cargo test -p tycho-execution --test protocol_integration_tests tessera
```

Then run the executor and router forks from `crates/tycho-execution/contracts` with
`forge test --match-path test/protocols/Tessera.t.sol -vv` (requires `BASE_RPC_URL`).

To reconcile a captured JSONL replay against the on-chain treasury at its closing block, run
`RPC_BASE=... python3 scripts/verify_balances.py replay.jsonl` from this package.
