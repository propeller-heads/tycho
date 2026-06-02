# Baseline / Mercury Tycho Integration Scope

Status: working scope document. Keep at repository root while implementing; remove or move before opening an upstream PR.

## Goal

Integrate the Baseline DEX / Mercury AMM with Tycho so Tycho users can discover, simulate, and eventually execute Baseline swaps.

Production coverage needs Ethereum mainnet and Base today, with additional EVM chains expected later. Keep chain-specific configuration data-driven and avoid naming packages or protocol ids as Base-only unless they are explicitly test fixtures.

The first target is not the full production integration. The first target is proving a Tycho VM quote simulation for one real Base pool:

- Relay / proxy: `0xc81Fd894C0acE037d133aF4886550aC8133568E8`
- bToken / component id candidate: `0xFf8104251E7761163faC3211eF5583FB3F8583d6` (`REPPO`)
- reserve token: `0x0b3e328455c4059EEb9e3f84b5543F74E24e7E1b` (`VIRTUAL`)
- protocol system candidate: `vm:baseline`

Current Baseline deployment metadata from `packages/contracts/addressBook.ts`:

- Ethereum mainnet: relay/proxy `0xc81Fd894C0acE037d133aF4886550aC8133568E8`, deployment block `24920863`
- Base: relay/proxy `0xc81Fd894C0acE037d133aF4886550aC8133568E8`, deployment block `45070267`

## Local References

Tycho docs in this repo:

- `docs/for-dexs/protocol-integration/README.md`
- `docs/for-dexs/protocol-integration/indexing/2.-implementation.md`
- `docs/for-dexs/protocol-integration/simulation/README.md`
- `docs/for-dexs/protocol-integration/simulation/ethereum-solidity.md`
- `docs/for-dexs/protocol-integration/execution/README.md`
- `docs/for-dexs/protocol-integration/3.-testing.md`

Baseline / Mercury references:

- Mercury contracts: `/Users/indigo/projects/baseline/mercury`
- Baseline monorepo: `/Users/indigo/projects/baseline/baseline-monorepo`
- Main ABIs: `/Users/indigo/projects/baseline/baseline-monorepo/packages/contracts/abis/mercury`
- SDK swap methods: `/Users/indigo/projects/baseline/baseline-monorepo/packages/sdk/src/baseline-sdk.ts`
- Existing Ponder handlers:
  - `/Users/indigo/projects/baseline/baseline-monorepo/apps/indexer/src/bFactory.ts`
  - `/Users/indigo/projects/baseline/baseline-monorepo/apps/indexer/src/bSwap.ts`

## Current Findings

Tycho is now a monorepo. The integration surfaces are all in this repository:

- Substreams indexing: `protocols/substreams/`
- VM adapter Solidity: `protocols/adapter-integration/evm/`
- VM adapter runtime registration: `crates/tycho-simulation/src/evm/protocol/vm/`
- Rust execution encoders: `crates/tycho-execution/src/encoding/evm/swap_encoder/`
- Solidity executors: `crates/tycho-execution/contracts/src/executors/`

Baseline pools are not individual pool contracts. Mercury stores pool state in the relay/proxy, namespaced by bToken. The bToken is the natural Tycho `ProtocolComponent.id`, but the storage owner for protocol state is the relay/proxy.

Mercury exposes canonical quote and swap entrypoints on the relay/proxy:

- `quoteBuyExactIn(bToken, reservesIn)`
- `quoteBuyExactOut(bToken, amountOut)`
- `quoteSellExactIn(bToken, amountIn)`
- `quoteSellExactOut(bToken, reservesOut)`
- `buyTokensExactIn(bToken, amountIn, limitAmount)`
- `buyTokensExactOut(bToken, amountOut, limitAmount)`
- `sellTokensExactIn(bToken, amountIn, limitAmount)`
- `sellTokensExactOut(bToken, amountOut, limitAmount)`

Because the canonical Solidity quote logic exists, start with a Tycho VM integration. Native Rust simulation should remain a fallback only if VM state indexing proves too broad or unstable.

## Tracing Fixture

Use a Base RPC with `debug_traceCall` support for tracing. In local commands, set it as:

`BASE_RPC_URL=<base-rpc-with-debug-tracecall>`

Public `https://mainnet.base.org` supports basic calls and `eth_createAccessList` with explicit transaction fields, but not `debug_traceCall`.

Useful live calls:

```bash
cast call 0xc81Fd894C0acE037d133aF4886550aC8133568E8 \
  'quoteBuyExactIn(address,uint256)(uint256,uint256,uint256)' \
  0xFf8104251E7761163faC3211eF5583FB3F8583d6 \
  1000000000000000000 \
  --rpc-url "$BASE_RPC_URL"

cast call 0xc81Fd894C0acE037d133aF4886550aC8133568E8 \
  'quoteSellExactIn(address,uint256)(uint256,uint256,uint256)' \
  0xFf8104251E7761163faC3211eF5583FB3F8583d6 \
  1000000000000000000 \
  --rpc-url "$BASE_RPC_URL"
```

Pinned fixture block:

- `46767428`

Observed quote sanity checks at the pinned block:

- `quoteBuyExactIn(REPPO, 1e18)` returns about `42.12 REPPO`
- `quoteSellExactIn(REPPO, 1e18)` returns about `0.0231 VIRTUAL`
- `getCurveParams(REPPO)` succeeds against the relay

Milestone 1 should pin an exact block before tests are written. The current traced block neighborhood is useful for discovery, but not precise enough for deterministic fixtures.

## Quote VM Surface

`debug_traceCall` with `prestateTracer` confirms the quote path is compact.

Contracts needed for `quoteBuyExactIn` / `quoteSellExactIn`:

- `0xc81Fd894C0acE037d133aF4886550aC8133568E8`: Baseline relay/proxy, storage owner
- `0x040c1abc3b2e89916e6bc30a043818a4ee58cad0`: BSwap implementation/component reached through `Relay.routes`
- Base helper/predeploy accounts that appear in the trace:
  - `0x4200000000000000000000000000000000000011`
  - `0x4200000000000000000000000000000000000019`
  - `0x420000000000000000000000000000000000001a`
  - `0x420000000000000000000000000000000000001b`

Quote storage needed on the relay/proxy:

- `State.Maker[REPPO]` offsets `0..3`
- `State.Pool[REPPO]` offsets `1..4`
- `State.BlockPricing[REPPO]` offsets `0..3`
- `Relay.routes[quoteBuyExactIn]`
- `Relay.routes[quoteSellExactIn]`

Decoded namespace bases for REPPO:

- `State.Pool[REPPO]` base:
  `0x2e5782ff7106dc4a2e670c6d4aef5214cb35a29a5b6b79349b1e2784d16a59b0`
- `State.Maker[REPPO]` base:
  `0x0217c68bf4a5896c7f6c388017f1ad1cec226dd09f05861e41f806a30ccceff4`
- `State.BlockPricing[REPPO]` base:
  `0x89ffd149630886dd330aa0f419cfb524fb4ef9042a961df5b9e2a34c8a7f6961`

Route slots:

- `Relay.routes[quoteBuyExactIn(address,uint256)]`:
  `0x955d26c667aa9032dd9a3eb06af8e5977d81c49e4856486032289f72a75cc881`
- `Relay.routes[quoteSellExactIn(address,uint256)]`:
  `0xb0b7d5e9e899acb486d97dbcc8a58f7bdb9f3c5c787dfda4e8e24c479c21e7de`

Both route slots currently resolve to:

`0x040c1abc3b2e89916e6bc30a043818a4ee58cad0`

## Execution Surface Notes

Execution is broader than quote simulation. Reverted access-list probes for `buyTokensExactIn` and `sellTokensExactIn` showed additional reads from:

- `State.Pool[REPPO]` through at least offsets `0..7`
- `State.Hook[REPPO]`
- `State.Meta`
- bToken storage
- reserve token storage
- `Relay.routes[buyTokensExactIn]`
- `Relay.routes[sellTokensExactIn]`
- external contracts `0x040c...cad0` and `0x7bab...e2db`

Route slots:

- `Relay.routes[buyTokensExactIn(address,uint256,uint256)]`:
  `0xba2a68675c12e923b90d2254c236ccc16758cdc81836d124aac4917cdec75690`
- `Relay.routes[sellTokensExactIn(address,uint256,uint256)]`:
  `0xf3218f98fa8800081550acb08352c45c7a27d57acb45bac26960e4ef9705bf69`

Tycho execution has an important constraint: Solidity executors should not transfer ERC20s directly. The Dispatcher / TransferManager handles ERC20 transfers. The Baseline executor should therefore describe where funds are expected and then call Mercury with the Dispatcher-managed token balances in place.

## Milestone 1: VM Quote Simulation Spike

Goal: get a Tycho VM simulation quote working for the REPPO/VIRTUAL fixture.

Deliverables:

- `BaselineSwapAdapter.sol` under `protocols/adapter-integration/evm/src/baseline/`
- `manifest.yaml` for the adapter
- Pinned REPPO quote fixture:
  - exact Base block number
  - `debug_traceCall` prestate JSON
  - relay storage slot values
  - code for relay/proxy, BSwap implementation/component, and any required predeploy/helper account
  - expected `quoteBuyExactIn` and `quoteSellExactIn` outputs at that block
- Foundry tests for the adapter against a Base fork or pinned prestate fixture
- Runtime bytecode generated into `crates/tycho-simulation/src/evm/protocol/vm/assets/`
- `baseline` registered in `crates/tycho-simulation/src/evm/protocol/vm/constants.rs`
- A minimal simulation test proving:
  - reserve -> bToken calls `quoteBuyExactIn`
  - bToken -> reserve calls `quoteSellExactIn`
  - simulated amounts match live `cast call` within exact expected rounding

Adapter behavior:

- Interpret `poolId` as bToken address encoded into `bytes32`.
- Determine direction using `sellToken` / `buyToken`.
- For reserve -> bToken:
  - sell side should use `quoteBuyExactIn(bToken, amountIn)`
- For bToken -> reserve:
  - sell side should use `quoteSellExactIn(bToken, amountIn)`
- Milestone 1 is exact-in only unless Tycho's VM registration or tests require exact-output support. Mark adapter capabilities accordingly. Trace and add `quoteBuyExactOut` / `quoteSellExactOut` later.

Non-goals for milestone 1:

- Full Substreams package
- Production indexing of every Baseline pool
- Production execution
- Native reserve path
- Native Rust simulation math

Decision gate:

- If the VM quote test cannot run from indexed relay storage, implementation should pause and reassess native Rust simulation.

## Milestone 2: Substreams Indexing

Goal: index Baseline/Mercury components and the VM state needed for quote simulation.

Recommended starting point:

- Use the singleton-style Substreams template, not a pure factory-per-pool template.
- Reason: pools are keyed by bToken, but protocol state lives inside the fixed relay/proxy contract.

Likely package name:

- `protocols/substreams/ethereum-baseline`

Chain configs should cover at least:

- Ethereum mainnet from block `24920863`
- Base from block `45070267`

Use chain-specific YAML files if this matches the Tycho Substreams pattern, for example:

- `ethereum-baseline.yaml`
- `base-baseline.yaml`

Events to track:

- `BFactory:PoolCreated`
- `BSwap:Swap`
- Any events that mutate pool/maker/block-pricing state but are not swaps
- Relay/component upgrade events if route implementations can change

Deployment boundaries still need to be pinned:

- Ethereum and Base factory/relay event address handling. The current Baseline address book lists the same deterministic relay/proxy on both chains.
- Whether future chains use the same deterministic relay/proxy address and route layout.
- Whether there are multiple Mercury relays/factories on any supported chain.
- production start block for the Substreams package

Component creation:

- Component id: bToken address string
- Protocol system: `vm:baseline`
- Tokens: `[bToken, reserveAddress]`
- Contract addresses should include at least:
  - relay/proxy
  - BSwap implementation/component contract
  - relevant Base proxy helper/predeploy if Tycho VM requires the code present

State requirements:

- Emit absolute ERC20 balances for bToken and reserve liquidity where Tycho expects component balances.
- Emit storage changes for all relay slots needed by quote simulation.
- Map each quote-relevant relay slot to its source of truth:
  - event-derived updates if available
  - deterministic derivation from `PoolCreated` arguments where valid
  - DCI/RPC storage reads if events are insufficient
- Index route slots as mutable relay storage unless route immutability is proven.
- Ensure implementation/component code is available to VM simulation.
- Explicitly test whether `0x040c...cad0` and the `0x4200...` helper/predeploy accounts belong in `contract_addresses` or in VM stateless contract attributes (`stateless_contract_addr_N` / `stateless_contract_code_N`). Do not treat Base predeploy/helper accounts as component contracts unless the VM requires it.

Open indexing question:

- Does Tycho need component balances owned by the relay, by the bToken component id, or by an explicit `balance_owner` state attribute for this protocol? The VM decoder supports a `balance_owner` state attribute; test against the REPPO fixture before broadening.

## Milestone 3: Execution

Goal: allow Tycho execution to call Mercury swaps onchain.

Files likely involved:

- `crates/tycho-execution/contracts/src/executors/BaselineExecutor.sol`
- `crates/tycho-execution/contracts/test/protocols/Baseline.t.sol`
- `crates/tycho-execution/src/encoding/evm/swap_encoder/baseline.rs`
- `crates/tycho-execution/src/encoding/evm/swap_encoder/mod.rs`
- `crates/tycho-execution/src/encoding/evm/swap_encoder/swap_encoder_registry.rs`
- `crates/tycho-execution/config/executor_addresses.json`
- `crates/tycho-execution/config/test_executor_addresses.json`
- Possibly `crates/tycho-execution/config/protocol_specific_addresses.json`

Executor behavior:

- ERC20-only initially.
- reserve -> bToken should call `buyTokensExactIn(bToken, amountIn, minOut)`.
- bToken -> reserve should call `sellTokensExactIn(bToken, amountIn, minOut)`.
- Before coding the executor, run a mini-spike to confirm Mercury's spender and funds location under Tycho execution:
  - whether Mercury pulls from `msg.sender`, executor, dispatcher/router, or another address
  - which address must hold input tokens
  - which address must approve the relay
  - which Tycho `TransferType`, `getTransferData`, and `fundsExpectedAddress` behavior matches that flow
- Do not implement native ETH reserve support in the initial executor.

Execution test requirements:

- Unit tests for Rust encoding.
- Solidity executor tests with mocked or forked Mercury calls.
- Full Base fork execution against REPPO/VIRTUAL once allowances/funding are correctly modeled.

## Milestone 4: Broaden Coverage

Only after REPPO quote simulation and executor tests are green:

- Index all Base Mercury pools from `PoolCreated`.
- Confirm storage surface across multiple pools and reserves.
- Add exact-output simulation if deferred.
- Add native reserve support only if a real pool needs it and Tycho transfer semantics are clear.
- Add maintenance handling for route/component upgrades.

## Test Plan

Substreams / indexing:

- Pool creation creates a Tycho component with id = bToken.
- Component tokens are `[bToken, reserve]`.
- Component contract addresses include all VM-required contracts.
- Swap or state-mutating events update balances and storage.
- Indexed storage state can reproduce the traced REPPO quote prestate.

Simulation:

- Adapter Foundry tests for token direction validation.
- Adapter Foundry tests for `getTokens`, `getLimits`, `getCapabilities`.
- Tycho simulation test for REPPO reserve -> bToken quote.
- Tycho simulation test for REPPO bToken -> reserve quote.
- Compare VM simulation output to live `quote*` calls at the same block.

Execution:

- Rust encoder test verifies encoded bToken, direction, token in/out, limits.
- Solidity executor test verifies correct Mercury method is called for each direction.
- Fork test verifies the full encoded calldata executes against Base.

## Risks

- Storage surface may be larger for execution than for quotes.
- Route/component upgrades can change implementation addresses and code.
- `BlockPricing` state depends on block number and in-block flow accumulators; quote tests must use block-compatible fixtures.
- ERC20 balances and allowances may require token-specific slot detection for execution tests.
- Baseline’s relay/proxy architecture is singleton-like, which may require custom Substreams storage tracking instead of relying on factory template defaults.
- Base predeploy/proxy helper accounts appear in prestate traces; confirm whether Tycho VM actually needs them loaded or whether they only appear because of proxy/predeploy resolution.

## Out of Scope For Initial PR

- Native Rust Mercury math
- Native reserve / ETH path
- Cross-chain deployments
- Staking, lending, credit, and BLV analytics
- Replacing the existing Baseline Ponder indexer
- Broad aggregator routing performance work

## Open Questions

- What exact block should be pinned for REPPO tests?
- Should `poolId` be raw bToken address string or canonical 32-byte address encoding? Adapter and Substreams must agree.
- Does Tycho prefer static attributes for relay address, reserve address, and implementation address, or should they be inferred from component contracts?
- Can route implementation code be indexed as a normal contract, or should it be emitted as a stateless contract attribute?
- Which Tycho transfer type best matches Mercury execution when the relay pulls tokens via `transferFrom`?
- Which Baseline events beyond `Swap` can mutate quote-relevant `Pool`, `Maker`, or `BlockPricing` state?

## Go / No-Go Criteria

Go for VM integration when:

- REPPO quote VM simulation matches live `quoteBuyExactIn` and `quoteSellExactIn`.
- The indexed relay storage surface remains compact and derivable from bToken.
- The adapter can be registered through existing Tycho VM runtime conventions.

Pause and reassess native Rust simulation when:

- Quote calls require dynamic external storage not emitted by Baseline events.
- Route/component code cannot be made available to the Tycho VM cleanly.
- Block pricing makes same-block simulation impossible from indexed state.
