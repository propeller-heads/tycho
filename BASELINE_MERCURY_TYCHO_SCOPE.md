# Baseline / Mercury Tycho Integration Scope

Status: working handoff document. Keep at repository root while implementing; remove or move before opening an upstream PR.

## Current State

We are integrating the Baseline DEX / Mercury AMM with Tycho so Tycho users can discover, simulate, and eventually execute Baseline swaps.

Current branch:

- `feat/baseline-adapter`

Recent checkpoint commit:

- `e3d7f058b test: add baseline substreams fixtures`

That commit added/updated:

- `protocols/substreams/ethereum-baseline/integration_test.tycho.yaml`
- `protocols/substreams/ethereum-baseline/integration_test_base_baseline.tycho.yaml`
- `protocols/substreams/ethereum-baseline/src/modules.rs`
- `protocols/testing/src/test_runner.rs`
- `protocols/testing/run.Dockerfile`
- `check-base-storage-range.sh`

Only known unrelated local file:

- `.antigravitycli/` is untracked and should not be included unless the user explicitly asks.

## Strategy Update

The original plan was Base-first using the REPPO pool. That is currently blocked for full Tycho VM/account hydration because every Base RPC tested so far returns `null` for `debug_storageRangeAt`.

The current strategy is:

- Use **Ethereum mainnet** as the full range-test and VM tracing target for now.
- Keep **Base** covered by an indexing-only fixture until a Base archive/debug RPC returns a real `debug_storageRangeAt` object.
- Do not treat the Base RPC blocker as a reason to stop Ethereum mainnet simulation work.

Production coverage still needs both Ethereum mainnet and Base, with more EVM chains expected later. Keep protocol ids and package names chain-neutral: the label should be `baseline`, not `baseline_mercury`.

## Repositories

Tycho integration repo:

- `/Users/indigo/projects/baseline/tycho-indexer`

Mercury contracts:

- `/Users/indigo/projects/baseline/mercury`

Baseline monorepo:

- `/Users/indigo/projects/baseline/baseline-monorepo`

Important Baseline references:

- ABIs: `/Users/indigo/projects/baseline/baseline-monorepo/packages/contracts/abis/mercury`
- SDK swap methods: `/Users/indigo/projects/baseline/baseline-monorepo/packages/sdk/src/baseline-sdk.ts`
- Existing Ponder handlers:
  - `/Users/indigo/projects/baseline/baseline-monorepo/apps/indexer/src/bFactory.ts`
  - `/Users/indigo/projects/baseline/baseline-monorepo/apps/indexer/src/bSwap.ts`

Use `cx` for code navigation where possible.

## Tycho Repo Surfaces

Tycho is now a monorepo. Relevant integration surfaces are in this repository:

- Substreams indexing: `protocols/substreams/`
- VM adapter Solidity: `protocols/adapter-integration/evm/`
- VM adapter runtime registration: `crates/tycho-simulation/src/evm/protocol/vm/`
- Rust execution encoders: `crates/tycho-execution/src/encoding/evm/swap_encoder/`
- Solidity executors: `crates/tycho-execution/contracts/src/executors/`

Docs are local in this repo:

- `docs/for-dexs/protocol-integration/README.md`
- `docs/for-dexs/protocol-integration/indexing/2.-implementation.md`
- `docs/for-dexs/protocol-integration/simulation/README.md`
- `docs/for-dexs/protocol-integration/simulation/ethereum-solidity.md`
- `docs/for-dexs/protocol-integration/execution/README.md`
- `docs/for-dexs/protocol-integration/3.-testing.md`

## Baseline Architecture

Baseline/Mercury pools are not individual pool contracts. Mercury stores pool state in the shared relay/proxy, keyed by bToken.

Relay/proxy:

- `0xc81Fd894C0acE037d133aF4886550aC8133568E8`

Deployment metadata from Baseline:

- Ethereum mainnet relay/proxy deployment block: `24920863`
- Base relay/proxy deployment block: `45070267`

Natural Tycho component model:

- Component id: bToken address string.
- Tokens: `[bToken, reserve]`.
- Protocol system/type label: `baseline`.
- VM storage owner: relay/proxy, not the bToken.

Canonical quote entrypoints on the relay/proxy:

- `quoteBuyExactIn(bToken, reservesIn)`
- `quoteBuyExactOut(bToken, amountOut)`
- `quoteSellExactIn(bToken, amountIn)`
- `quoteSellExactOut(bToken, reservesOut)`

Canonical swap entrypoints:

- `buyTokensExactIn(bToken, amountIn, limitAmount)`
- `buyTokensExactOut(bToken, amountOut, limitAmount)`
- `sellTokensExactIn(bToken, amountIn, limitAmount)`
- `sellTokensExactOut(bToken, amountOut, limitAmount)`

Because canonical Solidity quote logic exists, continue with a Tycho VM integration first. Native Rust simulation is fallback only if indexed VM state is too broad or unstable.

## Current Substreams State

Package:

- `protocols/substreams/ethereum-baseline`

Current module flow:

- `map_protocol_components`
  - Decodes `BFactory:PoolCreated`.
  - Creates Tycho protocol components.
- `store_pool_reserves`
  - Stores reserve token address by component id.
- `map_relative_component_balance`
  - Emits initial balances from `PoolCreated`.
  - Emits swap balance deltas from `BSwap:Swap`.
- `store_component_balances`
  - Aggregates relative deltas into absolute balances.
- `map_protocol_changes`
  - Aggregates components/balances.
  - Extracts relay/proxy storage changes.

Important fix in `e3d7f058b`:

- `map_protocol_changes` Rust input order now matches `substreams.yaml`.
- Correct order is:
  - params
  - block
  - `map_protocol_components`
  - `map_relative_component_balance`
  - `store_component_balances` deltas

Without this, Substreams panicked by decoding the wrong protobuf type as `StoreDeltas`.

## Test Fixtures

### Ethereum Mainnet

Fixture:

- `protocols/substreams/ethereum-baseline/integration_test.tycho.yaml`

Current mainnet bToken:

- `0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63`

Reserve:

- WETH `0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2`

Pool creation tx:

- `0xe21a7f343e28c0dba205757179c92b7e99d38dc4874e4b59c7f4af5792b53a29`

Test range:

- start block `24929812`
- stop block `24929814`

This fixture keeps relay hydration enabled:

```yaml
initialized_accounts:
  - "0xc81Fd894C0acE037d133aF4886550aC8133568E8"
```

Passing validation:

```bash
cd /Users/indigo/projects/baseline/tycho-indexer/protocols/testing
set -a; source /Users/indigo/projects/baseline/tycho-indexer/.substreams.env; set +a
PATH="/Users/indigo/projects/baseline/tycho-indexer/target/debug:$PATH" \
RPC_URL="<ethereum-mainnet-rpc-with-debug_storageRangeAt>" \
RUST_LOG=protocol_testing=info,tycho_client=error,tycho_indexer=error,error \
cargo run -- range --package ethereum-baseline --chain ethereum --match-test test_mainnet_pool_creation
```

Observed result:

```text
✅ test_mainnet_pool_creation passed
Passed 1/1
```

### Base

Fixture:

- `protocols/substreams/ethereum-baseline/integration_test_base_baseline.tycho.yaml`

Clone mapping:

- `base-baseline -> ethereum-baseline`
- Added in:
  - `protocols/testing/src/test_runner.rs`
  - `protocols/testing/run.Dockerfile`

Base REPPO bToken:

- `0xff8104251e7761163fac3211ef5583fb3f8583d6`

Reserve:

- `0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b`

Pool creation tx:

- `0x7c2b4d0dc9bce17b44e60618e6423cb2c081cf60b6f8e4cf93e23aa61f15e0e2`

Test range:

- start block `46596026`
- stop block `46596028`

Base is currently indexing-only:

```yaml
initialized_accounts: []
skip_simulation: true
skip_execution: true
```

Reason:

- Base RPCs tested so far return `null` for `debug_storageRangeAt`, including Chainstack/QuickNode/RouteMesh/Chainnodes endpoints.
- Direct `map_protocol_components` does decode the Base pool creation correctly.
- Full `map_protocol_changes` for the REPPO fixture needs about 3.05M blocks of store preparation from the Base package initial block, so it is not currently a fast range-test path.

Helper script for checking Base RPCs:

```bash
./check-base-storage-range.sh "https://your-base-rpc-url"
```

The script checks:

- `web3_clientVersion`
- historical block lookup for block `46596026`
- `debug_storageRangeAt` on the Baseline relay at that historical block
- `debug_storageRangeAt` on the Base reserve token at latest block

## Mainnet Quote Trace Findings

Traces were run at Ethereum mainnet fixture stop block:

- block number `24929814`
- block hex `0x17c6616`

Mainnet bToken:

- `0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63`

Relay:

- `0xc81Fd894C0acE037d133aF4886550aC8133568E8`

Delegate implementation seen in quote traces:

- `0xafaa95adb26fcd9094b46055a485f1fd6127c058`

Quote sanity checks:

```bash
cast call --rpc-url "$ETH_RPC_URL" \
  0xc81Fd894C0acE037d133aF4886550aC8133568E8 \
  'quoteBuyExactIn(address,uint256)(uint256,uint256,uint256)' \
  0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63 \
  1000000000000000
```

Observed:

- tokens out: `706215741364439014`
- fees: `17392602136080`
- slippage: `17700754405810894`

```bash
cast call --rpc-url "$ETH_RPC_URL" \
  0xc81Fd894C0acE037d133aF4886550aC8133568E8 \
  'quoteSellExactIn(address,uint256)(uint256,uint256,uint256)' \
  0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63 \
  1000000000000000000
```

Observed:

- reserves out: `1388167572774294`
- fees: `3201454632498`
- slippage: `2301348366222010`

Exact-out quote sanity checks also succeed:

- `quoteBuyExactOut(bToken, 1e18)` returns:
  - amount in `1415997227960769`
  - fees `24627057509087`
  - slippage `17700278238186195`
- `quoteSellExactOut(bToken, 1e15)` returns:
  - tokens in `720374257314260034`
  - fees `2306544890721`
  - slippage `2301532207151263`

### Prestate Trace Surface

`debug_traceCall` with Geth `prestateTracer` shows a compact read surface.

Accounts touched by all quote paths:

- `0x0000000000000000000000000000000000000000`
- `0x396343362be2a4da1ce0c1c210945346fb82aa49`
- `0xafaa95adb26fcd9094b46055a485f1fd6127c058`
- `0xc81fd894c0ace037d133af4886550ac8133568e8`

Relevant accounts:

- relay/proxy: `0xc81fd894c0ace037d133af4886550ac8133568e8`
- implementation/delegate target: `0xafaa95adb26fcd9094b46055a485f1fd6127c058`

No external ERC20 calls appeared in quote traces.

### Relay Storage Read Groups

Quote traces read bounded relay storage groups:

- `State.Pool[bToken]`
- `State.Maker[bToken]`
- `State.BlockPricing[bToken]`
- relay selector/route implementation slots

Mainnet decoded namespace bases for bToken `0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63`:

`State.Pool[bToken]`:

- root: `0x5e0c90d658953447a9a6183fafee6ce210d189b3996a6a327daf1c089411a92d`
- mapping base: `0xf1cd69f10b5666b5159332deb03b47dda410f99f82374307d8483c61b3bad849`
- quote reads include offsets:
  - `+1`
  - `+2`
  - `+4`

`State.Maker[bToken]`:

- root: `0xe1f3e5d5ea876721e96ead31698de4d68b16ce3ec39e86844058729c52a8c4a0`
- mapping base: `0x2658b70392a30648b69974e98d45f89daefd40ea12a8f3563cbc2854965b3945`
- quote reads include offsets:
  - `+0`
  - `+1`
  - `+2`
  - `+3`

`State.BlockPricing[bToken]`:

- root: `0xd5b8512e303453ba959118773f2c71ab3dc2c27edcc544a7dd32dc7c10b70d26`
- mapping base: `0x82c7bebcf900a18034da10a13744062338e36704ff08b16ff2a828c093424e2b`
- quote reads include offsets:
  - `+0`
  - `+1`
  - `+2`
  - `+3`

Selector/route slots observed:

- `quoteBuyExactIn`: `0x955d26c667aa9032dd9a3eb06af8e5977d81c49e4856486032289f72a75cc881`
- `quoteSellExactIn`: `0xb0b7d5e9e899acb486d97dbcc8a58f7bdb9f3c5c787dfda4e8e24c479c21e7de`
- `quoteBuyExactOut`: `0x25773b9d1ee969f5b3b202306c0517a181ebb3c75627c81bbefb09307b910c38`
- `quoteSellExactOut`: `0x2e78c2448d1fb73e484badd6310fa43918af4a230c23fefaa0e0b9ffb4a36e8c`

At the traced block, route slots resolve to implementation:

- `0xafaa95adb26fcd9094b46055a485f1fd6127c058`

### Trace Interpretation

The VM quote surface looks practical:

- Relay storage reads are compact and deterministic by bToken.
- Implementation code is a single delegate target at the traced block.
- Quote paths do not call token contracts.

Main risk:

- The Substream currently extracts all relay storage writes, but component updates are primarily marked from balance deltas. If `Pool`, `Maker`, or `BlockPricing` changes without a `BSwap:Swap` balance delta, Tycho may store fresh relay storage but not know which component should be refreshed.

Next implementation task:

- Identify all Mercury events/state-changing paths that mutate quote-relevant `Pool`, `Maker`, or `BlockPricing` state.
- Ensure those changes mark the affected bToken component updated.

## Current Milestones

### Milestone A: Substreams Scaffold And Fixtures

Status: committed in `e3d7f058b`.

Done:

- Mainnet fixture passes range test.
- Base fixture preserved as indexing-only.
- `base-baseline -> ethereum-baseline` clone mapping is wired in local and Docker protocol testing.
- Base RPC helper script added.

Remaining indexing questions:

- Whether component update marking is complete for all quote-relevant non-swap state changes.
- Whether Tycho needs a `balance_owner` attribute for the relay-held liquidity model, or whether current component balance output is enough for simulation/routing.
- Whether the implementation/delegate target should be emitted as a normal contract or as stateless VM metadata.

### Milestone B: VM Quote Simulation

Goal: get Tycho VM simulation quote working for the Ethereum mainnet bToken fixture.

Likely files:

- `protocols/adapter-integration/evm/src/baseline/BaselineSwapAdapter.sol`
- `protocols/adapter-integration/evm/manifest.yaml`
- `crates/tycho-simulation/src/evm/protocol/vm/constants.rs`
- `crates/tycho-simulation/src/evm/protocol/vm/assets/`
- VM simulation tests in the existing Tycho simulation test structure.

Adapter behavior:

- Interpret component/pool id as the bToken address.
- reserve -> bToken:
  - exact-in quote uses `quoteBuyExactIn(bToken, amountIn)`.
- bToken -> reserve:
  - exact-in quote uses `quoteSellExactIn(bToken, amountIn)`.
- Exact-out functions are traced and available:
  - reserve -> bToken exact-out uses `quoteBuyExactOut`.
  - bToken -> reserve exact-out uses `quoteSellExactOut`.

Expected adapter metadata:

- `getTokens` should return `[bToken, reserve]`.
- `capabilities` should match whichever sides are implemented.
- `getLimits` can stay conservative if existing VM adapters use broad limits.

Milestone B test goal:

- Unskip simulation for the Ethereum mainnet fixture.
- Compare VM simulation output to live `quote*` at block `24929814`.

### Milestone C: Execution

Goal: allow Tycho execution to call Mercury swaps onchain.

Likely files:

- `crates/tycho-execution/contracts/src/executors/BaselineExecutor.sol`
- `crates/tycho-execution/contracts/test/protocols/Baseline.t.sol`
- `crates/tycho-execution/src/encoding/evm/swap_encoder/baseline.rs`
- `crates/tycho-execution/src/encoding/evm/swap_encoder/mod.rs`
- `crates/tycho-execution/src/encoding/evm/swap_encoder/swap_encoder_registry.rs`
- `crates/tycho-execution/config/executor_addresses.json`
- `crates/tycho-execution/config/test_executor_addresses.json`
- Possibly `crates/tycho-execution/config/protocol_specific_addresses.json`

Initial execution scope:

- ERC20-only.
- reserve -> bToken calls `buyTokensExactIn(bToken, amountIn, minOut)`.
- bToken -> reserve calls `sellTokensExactIn(bToken, amountIn, minOut)`.
- Native reserve path is out of scope initially.

Before coding executor:

- Confirm Mercury's spender/funds location under Tycho execution:
  - whether the relay pulls from `msg.sender`, executor, dispatcher/router, or another address
  - which address must hold input tokens
  - which address must approve the relay
  - which Tycho `TransferType`, `getTransferData`, and `fundsExpectedAddress` behavior matches that flow

Execution is broader than quote simulation and may require additional token/bToken/reserve storage.

## Test Plan

Substreams/indexing:

- Mainnet `test_mainnet_pool_creation` passes.
- Base `test_reppo_pool_creation` remains indexing-only until Base RPC hydration works.
- Add tests or fixture checks for quote-relevant non-swap state changes once identified.

Simulation:

- Adapter Foundry tests for token direction validation.
- Adapter Foundry tests for `getTokens`, `getLimits`, `capabilities`.
- Tycho simulation test for mainnet reserve -> bToken quote.
- Tycho simulation test for mainnet bToken -> reserve quote.
- Compare VM simulation output to live `quote*` calls at block `24929814`.

Execution:

- Rust encoder test verifies encoded bToken, direction, token in/out, limits.
- Solidity executor test verifies correct Mercury method is called for each direction.
- Fork test verifies encoded calldata executes against Ethereum mainnet first.
- Add Base fork execution once a compatible Base RPC is available or another test harness avoids `debug_storageRangeAt`.

## Risks

- Base full VM/range testing is blocked on `debug_storageRangeAt` support.
- Route/component upgrades can change implementation addresses and code.
- `BlockPricing` state depends on block number and in-block flow accumulators; quote tests must use block-compatible fixtures.
- Quote-relevant state may change without swap balance deltas; component update marking must be audited.
- Execution surface is larger than quote surface.
- ERC20 balances and allowances may require token-specific slot detection for execution tests.
- Baseline’s singleton relay architecture requires custom storage tracking; factory template assumptions are not enough.

## Out Of Scope For Initial PR

- Native Rust Mercury math.
- Native reserve / ETH path.
- Cross-chain deployment completeness beyond Ethereum mainnet and Base scaffolding.
- Staking, lending, credit, and BLV analytics.
- Replacing the existing Baseline Ponder indexer.
- Broad aggregator routing performance work.

## Open Questions

- Which non-swap Baseline events mutate `State.Pool`, `State.Maker`, or `State.BlockPricing` for a bToken?
- Should route implementation code be emitted as a normal contract or as stateless VM metadata?
- Does Tycho need a `balance_owner` attribute for relay-held liquidity?
- Which exact VM simulation test harness should be used for the mainnet fixture?
- Which Tycho transfer type best matches Mercury execution when the relay pulls tokens via `transferFrom`?
- Can Base testing use a better RPC later, or do we need a Base-specific test-only path that avoids `debug_storageRangeAt`?

## Go / No-Go Criteria

Continue VM integration when:

- Mainnet quote VM simulation matches live `quoteBuyExactIn` and `quoteSellExactIn`.
- Indexed relay storage remains compact and bToken-derivable.
- Component update marking can cover quote-relevant state changes.
- Implementation code can be made available to Tycho VM cleanly.

Pause and reassess native Rust simulation when:

- Quote calls require dynamic external storage not emitted or derivable by Substreams.
- Route/component code cannot be made available to Tycho VM cleanly.
- Block pricing makes same-block simulation impossible from indexed state.
