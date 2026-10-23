# Tessera on Base

Indexes Tessera V (Wintermute's propAMM) on Base as `vm:tessera`. Each Pair contract is one
component, and consumers quote it by executing Tessera's own bytecode in the Tycho VM.

## Data flow

1. Replay Base from block **37,518,600**. A Pair is discovered when its EIP-1967
   implementation, base/quote tokens and Engine registration are written in the same
   committed transaction. Nested-call writes are folded by execution ordinal.
2. The component ID is the Pair address; tokens are sorted. TesseraSwap, the Engine and the
   Pair are stateful contracts, and their raw storage and code changes are indexed.
3. The implementation, pricing library and fee module are published as UTF-8 address
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

The fee module at Pair slot 52 is a view contract that returns a Pair's integrator fee for the
tag in the first byte of `swapData`. Execution sends empty `swapData`, which selects tag 0: the
fee the pause rules below watch.

## Storage-change map

`map_storage_changes` runs after `store_pairs` and emits, per successful transaction, the
committed writes owned by TesseraSwap, the Engine or a known Pair (including pairs created in
the same block), plus the tag-0 fee slot at any address. Writes are sorted by execution
ordinal. `store_safety` reads only this map; `map_relative_balances` and
`map_protocol_changes` read it for storage writes and the block for logs and raw contract
changes.

## Upgrade boundary

| Dependency | How it is followed | Assumption |
| --- | --- | --- |
| TesseraSwap | Fixed address, raw state/code | Slot 0 is Engine, slot 1 is treasury, swap/view ABI |
| Engine | Fixed address, raw state/code | Sorted-token registry mapping at slot 8, registration in the discovery transaction; `getTesseraPool`, `swapAmountView` and `fastPullBlock` ABI, compiled into the adapter |
| Pair implementation | EIP-1967 slot → attribute 0 + code | Pair proxy template, token slots 48/49; `poolState()`, `baseToken()` and `quoteToken()` ABI and the `poolState()` layout, compiled into the adapter from the SDK Tessera provides |
| Pricing library | Pair slot 51 → attribute 1 + code | Staticcalled by the implementation; reads no storage |
| Fee module | Pair slot 52 → attribute 2 + code | Tag-0 fee is zero |
| Treasury | TesseraSwap slot 1 → `balance_owner` and balance bridge; adapter constructor argument | Standard ERC20 and canonical WETH events; a rotation needs a new adapter runtime, or limits are capped by the old treasury |

A component is marked `paused` when its fee module's tag-0 fee is nonzero (the VM loads the
fee module's code but not its storage, so it would read that fee as zero), and every component
is paused once TesseraSwap points to a different Engine. A Pair is also paused when an Engine
registry write moves its token pair to another Pair or clears it. `paused` is never cleared.
Consumers do not act on `paused` yet.

The package pauses on a tag-0 fee rather than indexing the fee module's storage, which would let
a simulation price the fee, because the fee module cannot be linked to the component. A
component's
contracts are fixed when it is created, and tycho-client requests and forwards only component
contracts and DCI-traced accounts. A fee module assigned later, which can also rotate, would need
DCI, which Tycho does not run on Base.

The safety store records the tag-0 fee slot at every address from block 37,518,600, so a fee
module configured before it is assigned is still caught. A fee set before that block is not
recorded and reads as zero; the current fee module was deployed later, at 48,160,230. The store
covers every address because fee-module assignments are themselves derived from
`map_storage_changes`, so no module before it knows which addresses are fee modules. The cost is a
few forwarded writes per block from other contracts with the same mapping layout, such as Uniswap
V4's PoolManager.

TesseraSwap routes by token pair, not by Pair address, so a Pair the registry no longer points at
would quote and settle against its successor. The adapter therefore checks
`getTesseraPool(sell, buy)` on every quote, limit and swap and reverts with
`Unavailable("Pair not registered")` unless it returns that Pair. That check, not `paused`, is
what stops a replaced Pair today, and it lifts again if the registry points back.

A Pair the registry points at is discovered only when it is initialized and registered in the
same transaction. If the Engine is re-pointed to a Pair initialized earlier, that Pair is not
indexed: the old component stops quoting and the new one is missing.

## Tests

Unit tests cover discovery, the storage-change filter, balance deltas (seeds, self-transfers,
rotations, a zero treasury), safety-store writes, dependency attributes and pause conditions.
From `protocols/substreams`:

```sh
cargo test -p base-tessera
cargo clippy -p base-tessera --all-targets -- -D warnings
cargo build -p base-tessera --release --target wasm32-unknown-unknown
```

To reconcile a captured JSONL replay against the on-chain treasury at its closing block, run
`RPC_BASE=... python3 scripts/verify_balances.py replay.jsonl` from this package.
