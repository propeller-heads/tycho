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

## Storage-change map

`map_storage_changes` runs after `store_pairs` and emits, per successful transaction, the
committed writes owned by TesseraSwap, the Engine or a known Pair (including pairs created in
the same block), plus the tag-0 fee slot at any address. Writes are sorted by execution
ordinal. `store_treasury` and `store_safety` read only this map; `map_relative_balances` and
`map_protocol_changes` read it for storage writes and the block for logs and raw contract
changes.

## Upgrade boundary

| Dependency | How it is followed | Remaining assumption |
| --- | --- | --- |
| Swap | Fixed deployment address, raw state/code | Slot 0 is Engine, slot 1 is Treasury, swap/view ABI |
| Engine | Fixed deployment epoch, raw state/code | Sorted-token registry mapping at slot 8 and registration in discovery transaction |
| Pair implementation | EIP-1967 slot → attribute 0 → bytecode reload | Pair proxy template and token slots 48/49 |
| Pricing library | Pair slot 51 → attribute 1 → bytecode reload | Staticcalled by the implementation; reads no storage |
| Write helper | Pair slot 52 → attribute 2 → bytecode reload | Empty swapData/tag 0 has zero fee |
| Treasury | Swap slot 1 → owner attribute and balance bridge | Standard ERC20 and canonical WETH event accounting |
| Adapter view decoding | Pair `poolState()` / token getters | Current static ABI and 20-order ladder used only for search bounds |

Engine replacement pauses components. A nonzero helper tag-0 fee also pauses affected
components, because helper-owned state is not copied into the VM. To detect adopting
an already-configured helper, the safety store remembers the single tag-0 mapping slot
for candidate addresses from genesis; only assigned helpers trigger Tessera updates.
The substreams emits these pauses as the `paused` attribute and never clears it. Simulation
support for this signal is deferred: the VM consumer currently does not block quotes or
zero limits based on `paused`. Emitting the attribute alone does not stop routing.
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
2.42–2.57M gas in both WETH/USDC and cbBTC/USDC directions at block 50,548,423. Future
bytecode can change these costs. Choosing a higher gas budget or improving the search
is still necessary if approximately 1% precision is mandatory.

Price remains a finite difference of venue quotes. An exact-output quote selects an
input step large enough to represent at least 1,000 raw output units, reducing integer
rounding noise. No native pricing logic is introduced.

## Validation

- Substreams (2026-09-28): 32 unit tests; Clippy with warnings denied; release WASM and
  package build.
  Balance deltas, safety-store writes and protocol changes are tested through their pure
  functions: seed suppression, rotation bridging, helper/fee/engine recording, dependency
  attributes, pause conditions and pre-creation writes. Regression tests also cover missing
  storage transaction metadata and multiple treasury rotations with same-block pair seeding.
- Storage-change map replay: `map_protocol_changes` matched the previous module graph block
  for block, up to ordering of unordered collections, over `[37518600, 37518850)` (pair
  creation) and `[37519500, 37523500)` (2,589 transactions of pair/Engine updates). On the
  latter, alternating runs took 264 s vs 302 s and 387 s vs 507 s (new vs previous graph).
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
- Adapter fork (2026-09-28): ten tests at 50,548,423 cover WETH/USDC exact-in and exact-out both
  ways, cbBTC limits/price both ways, depleted-ladder hint, stale block, wrong tokens,
  gas budget, two fills, settled-output accounting and underfilled exact-output rejection.
- Executor fork: eight `TesseraExecutorTest` cases cover constructor checks, decoding and
  its length error, transfer data, funds address, a direct WETH→USDC swap settling the view
  quote, and decoding the Rust-generated USDC→WETH fixture.
- Router fork (2026-09-28): three tests through the real router → executor → venue path.
  Full Rust-generated router calldata covers WETH→USDC and USDC→WETH; both check exact
  recipient receipt, router residual balances, and empty swapData.
- Rust (2026-09-28): three encoder units and two full-calldata fixtures; six VM delta tests,
  four snapshot-decoder tests, and two offline Tessera consumer tests using rebuilt adapter
  bytecode (returned state and indexed-block freshness).
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

## VM and execution checks (2026-09-28)

The adapter reports the actual recipient balance increase for exact-input trades and rejects
an underfilled exact-output trade. Its embedded runtime is rebuilt from that source. Two
synthetic settlement tests model a 10% view/settlement discrepancy; the real fork tests keep
venue storage unchanged and fund the caller with test tokens.

Execution covers both WETH→USDC and USDC→WETH using Rust-generated full router calldata,
with an explicit empty-swapData call expectation and exact recipient balance checks. The
encoder uses Base chain context. Executor deployment configuration and its deterministic
test address are present; no production deployment is claimed.

The VM tests cover returned swap state, indexed-block freshness, dependency reload,
treasury-owner deltas, and rejected non-UTF-8 snapshot dependency addresses. The latter
must error rather than loop forever at the same attribute index. Pause handling is deferred.

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

Copy the resulting `out/TesseraSwapAdapter.sol/TesseraSwapAdapter.evm.runtime` to the
simulation VM assets after adapter changes. From the repository root:

```sh
cargo test -p tycho-simulation --lib test_tessera
cargo test -p tycho-simulation --lib evm::protocol::vm::state::tests::test_delta_transition
cargo test -p tycho-execution --lib tessera
```

Regenerate the router calldata with
`cargo test -p tycho-execution --test protocol_integration_tests tessera`, then run the
executor and router forks from `crates/tycho-execution/contracts` with
`forge test --match-path test/protocols/Tessera.t.sol -vv` (requires `BASE_RPC_URL`).

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
