# Tessera on Base — VM integration

Initial implementation against `docs/research/tessera-vm-integration-spec.md`, built from
main `bf1dd4d6fb0499b99ea8607fdf866040fd1de2cc`. This is an integration candidate;
production deployment and the remaining acceptance items below are outstanding.

## Data flow

1. Replay Base from block **37,518,600**. Discover a Pair only when its EIP-1967
   implementation, base/quote initialization and matching Engine registration appear
   in the same committed transaction. Fold nested-call writes by execution ordinal.
2. Publish the Pair address as component ID, sorted token addresses, and Swap,
   Engine and Pair as stateful contracts. Index their raw storage/code changes.
3. Publish implementation, pricing library and write helper as UTF-8 hex address
   attributes `stateless_contract_addr_0/1/2`. Follow slot writes for later changes.
   The VM now loads sparse dependency deltas before refreshing pool caches; initial
   snapshots and deltas share the bytecode loader. A failed code load fails the
   transition rather than deliberately falling back to the old implementation.
4. Set `balance_owner` to Swap slot 1. Seed each new (token, component) inventory
   with end-block `balanceOf`, then track transfers and canonical WETH wrap/unwrap
   events. On a treasury rotation, account old-owner events and bridge using
   `new_owner_end_balance - old_owner_end_balance`. New components seeded in that
   block skip the bridge and transfer deltas. RPC failure aborts instead of adding zero.
   The VM also refreshes `balance_owner` on deltas.
5. The Solidity adapter calls Tessera's view and actual swap entry points. The VM
   returns execution storage overwrites for subsequent fills. There is no SDK math
   translation, pricing-version registry, DCI or `debug_storageRangeAt` dependency.
6. The Base-only encoder emits the two token addresses (40 bytes). The executor
   uses the Swap entry point with **empty swapData** and the router's debit/allowance
   flow. It is registered in test deployment tooling, not deployed to production.

## Upgrade boundary

| Dependency | How it is followed | Remaining assumption |
| --- | --- | --- |
| Swap | Fixed deployment address, raw state/code | Slot 0 is Engine, slot 1 is Treasury, swap/view ABI |
| Engine | Fixed deployment epoch, raw state/code | Sorted-token registry mapping at slot 8 and registration in discovery transaction |
| Pair implementation | EIP-1967 slot → attribute 0 → bytecode reload | Pair proxy template and token slots 48/49 |
| Pricing library | Pair slot 51 → attribute 1 → bytecode reload | Target is delegatecalled; state lives in Pair |
| Write helper | Pair slot 52 → attribute 2 → bytecode reload | Empty swapData/tag 0 has zero fee |
| Treasury | Swap slot 1 → owner attribute and balance bridge | Standard ERC20 and canonical WETH event accounting |
| Adapter view decoding | Pair `poolState()` / token getters | Current static ABI and 20-order ladder used only for search bounds |

Engine replacement pauses components. A nonzero helper tag-0 fee also pauses affected
components, because helper-owned state is not copied into the VM. To detect adopting
an already-configured helper, the safety store remembers the single tag-0 mapping slot
for candidate addresses from genesis; only assigned helpers trigger Tessera updates.
These pauses fail closed and do not automatically resume when configuration changes back.
Layout/ABI changes and new external stateful dependencies still require integration work.

## Limits and price

`getLimits` sums the 20 orders only to bound a search. It obtains a small exact-output
quote as a lower bound, then runs two exact-input bisection probes. Seeding by output
precision avoids rounding one raw USDC to zero when buying cbBTC. The reported output
is a conservative hint; when only the seed succeeds, exact-output and exact-input
rounding need not be identical. `HardLimits` is not advertised.

**Spec deviation requiring acceptance:** two rounds can leave 25% of the original
interval unresolved and can substantially underestimate remaining liquidity. This
is not the approximately 1% precision requested by the spec. Eight rounds measured
6,779,237 gas in the WETH/USDC fixture; the current three-probe implementation measures
2.34–2.49M gas in both WETH/USDC and cbBTC/USDC directions at block 50,548,423. Future
bytecode can change these costs. Choosing a higher gas budget or improving the search
is still necessary if approximately 1% precision is mandatory.

Price remains a finite difference of venue quotes. An exact-output quote selects an
input step large enough to represent at least 1,000 raw output units, reducing integer
rounding noise. No native pricing logic is introduced.

## Validation (2026-09-24)

- Substreams: 9 unit tests; Clippy with warnings denied; release WASM and package build.
- Genesis live replay: `[37518600, 37519400)`, three pairs discovered. All six final
  token/component balances matched treasury `balanceOf` exactly at 37,519,399.
- Separate first-swap replay: `[37519370, 37519400)` completed; its four emitted
  token/component balances matched closing treasury balances exactly. The genesis
  range also covers creation, first price posts and the first swap.
- Treasury rotation replay `[37737340, 37737355)` remains **unverified live**. The
  server required 657,000 preparation blocks; a bounded 670,000-block request still
  had no output after five minutes and was stopped. The accounting unit test passes,
  but it is not a substitute for this historical acceptance check.
- Real testing harness: genesis discovery passed; **one VM snapshot decoded**. Genesis
  simulation is explicitly skipped due to the old implementation's gas-price branch;
  execution is skipped because no production executor is deployed. Generic balance
  checks use the wrong owner, so treasury balances were reconciled separately.
- Adapter fork: eight tests at 50,548,423 cover WETH/USDC exact-in and exact-out both
  ways, cbBTC limits/price both ways, depleted-ladder hint, stale block, wrong tokens,
  gas budget and two fills.
- Router fork: real full router → Tessera executor → venue swap, including Rust-generated
  calldata, empty swapData and exact recipient balance comparison.
- Rust: encoder's three tests; VM's six targeted delta tests; two offline Tessera consumer
  tests using actual returned `new_state` and advancing the actual indexed block.
- Execution crate's complete library suite: 200 passed, 3 ignored.
- Combined execution/simulation library suite did **not** complete green. Existing
  `test_engine_block_advance_invalidates_cached_limits` accesses `SHARED_TYCHO_DB` while
  its fixture initializes a separate DB (also present in the main baseline); isolated
  rerun reproduces the failure. `test_contract_deployment` also failed. Several unrelated
  RPC tests remained running; the suite was interrupted after more than seven minutes.

The offline fixture `crates/tycho-simulation/src/evm/protocol/vm/assets/tessera_50548423.json`
contains public Base bytecode/storage and two expected USDC→WETH view outputs. It was
captured with `debug_traceCall`'s prestate tracer plus `eth_getStorageAt` for Pair slots
0–52 and Swap slots 0–2; the second expected quote overrides Pair slot 3 by +100 USDC.
This is a test-fixture capture method, not an indexing runtime requirement. Tokens are
mocked by Tycho's normal ERC20 proxy in the consumer tests. Fork tests independently
exercise real token transfers.

## Reproduce

From `protocols/substreams`:

```sh
cargo test -p base-tessera
cargo clippy -p base-tessera --all-targets -- -D warnings
cargo build -p base-tessera --release --target wasm32-unknown-unknown
```

From `protocols/adapter-integration/evm` with `RPC_BASE` exported:

```sh
forge test --match-contract TesseraSwapAdapterTest -vv
bash scripts/buildRuntime.sh -c TesseraSwapAdapter -s 'constructor(address)' \
  -a 0x55555522005BcAE1c2424D474BfD5ed477749E3e
```

Copy the resulting `out/TesseraSwapAdapter.sol/TesseraSwapAdapter.evm.runtime` to the
simulation VM assets after adapter changes. From the repository root:

```sh
cargo test -p tycho-simulation --lib test_tessera
cargo test -p tycho-simulation --lib evm::protocol::vm::state::tests::test_delta_transition
cargo test -p tycho-execution --lib tessera
```

Run the router fork from `crates/tycho-execution/contracts` with
`forge test --match-contract TesseraRouterTest -vv` (requires `BASE_RPC_URL`; set it to the Base RPC endpoint).

## Standards review

The review found ordinal folding, overly broad helper triggers and duplicated fee-key
construction. All three were corrected and the limited follow-up review closed them.

## Spec review

The initial review identified limits losing small remaining liquidity and missing true
consumer tests for block context and returned state. Consumer tests now cover both.
The liquidity fix also covers cbBTC output rounding. The limits precision/gas tradeoff
above remains open. Production executor deployment remains intentionally outstanding.

For independent balance reconciliation, pass a captured JSONL replay to
`RPC_BASE=... python3 scripts/verify_balances.py replay.jsonl` from this package.
It verifies the balances actually present in that file against the on-chain treasury
at the closing block, and reports the count so partial-range coverage is explicit.
