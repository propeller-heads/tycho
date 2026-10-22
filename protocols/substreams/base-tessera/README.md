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
   attributes `stateless_contract_addr_0/1/2` and follow their slot writes. Each address comes
   with its closing-block code as `stateless_contract_code_0/1/2`, read with one batched
   `eth_call` per block (see `src/code.rs`), so consumers load dependencies without RPC. A
   failed code read aborts the block.
4. `balance_owner` is TesseraSwap slot 1 (the treasury). A new (token, component) is seeded
   with the treasury's closing `balanceOf`, then follows ERC20 transfers and canonical WETH
   wrap/unwrap events. On a treasury rotation, the block's events are counted against the
   previous treasury and `new_owner_balance - old_owner_balance` is bridged at the rotation.
   A failed `balanceOf` call aborts the block.

   Both reads are `eth_call`s. On a Flashblocks partial block an `eth_call` stalls the stream
   for about three minutes, so this package must run without `--enable-partial-blocks`.
5. The adapter quotes with the Engine's `swapAmountView`, settles with
   `tesseraSwapWithAllowances` and reports the settled amount. The VM keeps the swap's storage writes for later fills.
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
| Pair implementation | EIP-1967 slot → attribute 0 + code | Pair proxy template, token slots 48/49 |
| Pricing library | Pair slot 51 → attribute 1 + code | Staticcalled by the implementation; reads no storage |
| Write helper | Pair slot 52 → attribute 2 + code | Tag-0 fee is zero |
| Treasury | TesseraSwap slot 1 → `balance_owner` and balance bridge; adapter constructor argument | Standard ERC20 and canonical WETH events; a rotation needs a new adapter runtime, or limits are capped by the old treasury |
| Adapter | Pair `poolState()` and token getters, Engine `getTesseraPool`, `swapAmountView` and `fastPullBlock` | ABI and fill rules from the SDK Tessera provides; the contracts are unverified; the Engine and treasury are constructor arguments |

A component is marked `paused` when its write helper's tag-0 fee is nonzero (the VM loads the
helper's code but not its storage, so it would read that fee as zero), and every component is
paused once TesseraSwap points to a different Engine. A Pair is also paused when an Engine
registry write moves its token pair to another Pair or clears it. The safety store records the
tag-0 fee slot at every address from genesis, so a helper that was configured before assignment
is still caught. `paused` is never cleared. Consumers do not act on `paused` yet.

TesseraSwap routes by token pair, not by Pair address, so a Pair the registry no longer points at
would quote and settle against its successor. The adapter therefore checks
`getTesseraPool(sell, buy)` on every quote, limit and swap and reverts with
`Unavailable("Pair not registered")` unless it returns that Pair. That check, not `paused`, is
what stops a replaced Pair today, and it lifts again if the registry points back.

A Pair the registry points at is discovered only when it is initialized and registered in the
same transaction. If the Engine is re-pointed to a Pair initialized earlier, that Pair is not
indexed: the old component stops quoting and the new one is missing.

## Limits and price

`getLimits` computes the largest sell amount that fills in full from the Pair's `poolState()`,
following the venue's own fill rules: the sell side's orders up to the first dead one, skipping
the first `fastPullLevel` orders while the Engine's `fastPullBlock` is not yet passed, minus fills
since the operator last posted the ladder. With `reduceAccumulator` set, fills in the opposite
direction give capacity back; the venue converts them at the mid price, the adapter by selling
them back, which pays the spread and keeps the limit on the safe side. The result is capped so
the output fits the treasury's balance of the buy token, which the view quote ignores but
settlement needs. `limits[1]` is the quote at that amount. If
the computed amount does not quote (a stale price, or a `poolState()` layout that no longer
means what the adapter assumes), it falls back to an eight-probe search below it. Quotes go
through `Engine.swapAmountView(..., address(0))`, which matches what today's TychoRouter gets at
about 150k gas instead of a scan of the adapter's code; a limit costs about 300k gas.
`HardLimits` is not advertised.

`price` is a marginal price: a finite difference of view quotes whose step buys 10^6 raw output
units. The view rounds output down by under one unit, so the price is low by less than 10^-6. The
step is fixed in raw units because `decimals()` reads 0 for the VM's token proxies, and it stays
inside the top ladder level on the indexed pairs. A Pair that cannot quote, such as one past its
staleness cutoff or with a dead ladder, makes `price` revert, so it has no spot price instead of 0.

Quotes depend on the transaction's priority fee. Once `tx.gasprice` exceeds `block.basefee`
by more than an operator-set threshold (0.002 gwei at block 50,548,423, 0.01 gwei at
51,977,873), TesseraSwap applies a fixed haircut per pair, at any size and in both
directions: 4 bps on WETH/USDC and cbBTC/USDC, 10 bps on deSPXA/USDC, 2.5 bps on NVDAc/USDC
and 0.01 bps on the other pairs (measured at block 51,979,346). The Tycho VM simulates with
zero gas price and base fee, so quotes and limits are the zero-priority-fee price; a fill
that pays a priority fee above the threshold receives the haircut.

TesseraSwap also scans its caller's whole code on every settlement, at about 79 gas per byte.
The router is the caller on chain, the adapter in the VM, so `swap` adds
`ROUTER_SETTLEMENT_EXTRA_GAS` (1.36M) to the gas it measures: the scan of the TychoRouter
at `0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67` (22,390 bytes) beyond the adapter's own, plus
the storage the adapter's quote has already warmed. It matches the router's settlement gas
within 1% at block 52,890,608. The constant is tied to that router: a redeployment, or a solver
settling through its own contract, pays a different scan.

## Tests

- Substreams: unit tests for discovery, the storage-change filter, balance deltas (seed
  suppression, self-transfers, rotations), safety-store writes, dependency attributes and
  pause conditions.
- Adapter: Base forks at blocks 50,548,423 to 52,890,608 — both directions, exact in and out,
  price, staleness, two consecutive fills, settled-output accounting against a synthetic venue,
  a replaced or removed registry entry, limits under the fill rules and the treasury cap, and
  swap gas against the router's own settlement.
- Execution: encoder units, executor fork tests, and full Rust-generated router calldata in
  both directions.
- Simulation: VM delta tests for dependency reload and treasury rotation, snapshot decoding,
  and offline consumer tests on `vm/assets/tessera_50548423.json` (indexed-block freshness,
  registry replacement, a Pair that stopped quoting, treasury-capped limits and an upgrade's
  inline dependency code).

The offline fixture holds public Base bytecode and storage for one USDC→WETH quote. It was
captured with `debug_traceCall`'s prestate tracer plus `eth_getStorageAt` for Pair slots 0–52
and TesseraSwap slots 0–2.

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
bash scripts/buildRuntime.sh -c TesseraSwapAdapter -s 'constructor(address,address,address)' \
  -a 0x55555522005BcAE1c2424D474BfD5ed477749E3e,0x31e99E05fee3DCE580af777C3fD63eE1B3B40c17,0x3dBE077e7986657E95e1CC50089f17a5a4AF0AaE
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
