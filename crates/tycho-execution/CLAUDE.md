# Tycho Execution

DeFi swap execution framework: Solidity smart contracts (TychoRouterV3) + Rust encoding library. Multi-protocol token
swaps with fee-taking, vault-based accounting, and 20+ DEX integrations.

**Docs**: https://docs.propellerheads.xyz/tycho
**License**: BUSL-1.1 (Solidity), MIT (Rust)

## Solidity Architecture

```
TychoRouterV3 (entry point)
  inherits AccessControl           -- role-based admin (add executors, set fees)
  inherits Dispatcher              -- executor dispatch via delegatecall
    inherits TransferManager       -- input/output transfers, Permit2/ERC20/Vault funding
      inherits Vault (ERC6909)     -- multi-token vault, transient storage deltas
  inherits EIP712                  -- client fee signature verification

FeeCalculator (separate contract, called via staticcall -- read only)
```

### Swap Flow (end-to-end)

```
Entry (e.g. splitSwap)
  → input transfer (_transfer)
  → for each swap hop:
      balance snapshot (balanceOf before)
      → delegatecall executor.swap()
      balance snapshot (balanceOf after)
      → amountOut = diff (single source of truth)
      → if outputToRouter: forward to receiver via _transferOut
  → _takeFees (deduct client fee + router fees, credit vault balances)
  → _maybeAddClientContribution (cap slippage contribution)
  → _settleOutput (transfer/credit final amount to receiver or vault)
  → _finalizeBalances (verify all transient deltas settled)
```

### Core Contracts (`contracts/src/`)

| Contract                       | Purpose                                                                                                                                                                                                                                                        |
|--------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `TychoRouterV3.sol`              | Entry point. 3 swap strategies (single/sequential/split) x 3 funding modes (transferFrom/Permit2/vault) = 9 public methods. `_takeFees()` deducts fees, `_settleOutput()` transfers/credits final output to receiver or vault                                  |
| `Vault.sol`                    | ERC6909 multi-token vault (see subsection below)                                                                                                                                                                                                               |
| `Dispatcher.sol`               | Executor dispatch. 1-day timelock on new executors. Balance-diff verification of swap outputs. Queries transfer data via staticcall, executes swaps via delegatecall                                                                                           |
| `TransferManager.sol`          | Caps transferFrom to the declared input amount. `_transferOut` for output transfers (handles FoT/rebasing tokens via balance-diff). 6 transfer scenarios depending on context                                                                                  |
| `FeeCalculator.sol`            | Dual fee system: router fee on output + router fee on client fee. Per-client custom rates. Upgradeable without redeploying router                                                                                                                              |
| `uniswap_x/UniswapXFiller.sol` | Filler contract for UniswapX V2DutchOrder Reactor. Wraps TychoRouterV3: receives an order via `reactorCallback`, approves TychoRouterV3 to pull input tokens, calls TychoRouterV3, then approves the reactor to pull output. Single-order only; AccessControl-gated. |

Interfaces (`contracts/interfaces/`): `IExecutor` (swap [void],
getTransferData [returns transferType, receiver, tokenIn, tokenOut, outputToRouter],
fundsExpectedAddress), `ICallback` (handleCallback, verifyCallback, getCallbackTransferData), `IFeeCalculator` (
calculateFee [takes FeeInput → FeeRecipient[]], mustOutputThroughRouter [takes clientFeeBps, client → bool],
getAllClientFees [takes start, count → (address[] clients, CustomFees[] fees)]). Also
`IPropAMM` / `IPropAMMRouter` (the pAMM standard and Titan's fallback router) and `IUniversalRouter`.

### Vault (`Vault.sol`)

ERC6909 multi-token vault with dual storage:

**Transient storage** (tload/tstore, ~100 gas per op): Tracks per-token deltas (credits/debits) during a swap. Positive
delta when tokens arrive at the router, negative when they leave. `nonZeroDeltaCount` tracks unsettled deltas.

**Persistent storage** (ERC6909 balances): User token balances for deposits, withdrawals, and fee credits.

**Settlement** (`_finalizeBalances`): Called at the end of every swap. Validates:

- Non-vault swaps: all deltas must be zero (`nonZeroDeltaCount == 0`)
- Vault-funded swaps: at most one negative delta (the input token), which gets burned from the user's vault balance

**External methods**: `deposit(token, amount)` and `withdraw(token, amount)`. Supports native ETH via `address(0)`.

**Fee accounting**: Fees credited directly to fee receivers' vault balances via `_creditVault()` -- persistent storage
writes (~22k gas each) but no ERC20 transfers.

**Why transient storage is kept** (even with balance-diff verification): The delta system is a cheap (~100 gas per op)
safety guardrail that catches routing logic bugs and exploits. Example: a malicious encoder inserts a third split
through a compromised protocol whose callback tells TransferManager to transfer PEPE instead of the expected token. The
router would lose PEPE, but transient storage detects the negative PEPE delta and reverts. It also prevents overpayment
in vault-funded split swaps where split percentages don't sum to 100%.

### Fee System (`FeeCalculator.sol`)

Three fee layers, deducted from swap output:

1. **Client fee** (EIP-712 signed): Passed per-swap via `ClientFeeParams` struct
   containing `clientFeeBps`, `clientFeeReceiver`, `maxClientContribution`, `deadline`, and `clientSignature`. The
   client signs a `ClientFee` typehash that covers both the fee params **and** the full swap
   intent (`amountIn`, `tokenIn`, `tokenOut`, `expectedAmountOut`, `minAmountOut`, `receiver`, `swap`); the router verifies the EIP-712
   signature on-chain before applying any fee. Binding the signature to swap data (including the encoded swap bytes)
   prevents cross-swap replay attacks. `_isValidClientSignature` accepts two signature kinds: a 65-byte ECDSA signature
   recovering to `clientFeeReceiver`, or — when that fails — an ERC-1271 signature of any length that the
   `clientFeeReceiver` contract validates itself (`isValidSignature`, staticcalled via OpenZeppelin's
   `SignatureChecker`). ECDSA runs first so an EOA carrying delegated code (EIP-7702) keeps signing with its own key.
   Contract signatures are revocable — one that verifies in a given block may stop verifying later.
   The `clientFeeReceiver` address doubles as the client
   identifier. `maxClientContribution` caps how much the client contributes from their vault balance to cover a
   shortfall below `minAmountOut`. Passing zero `ClientFeeParams` is allowed (no fee, no client tracking).
2. **Router fee on output** (stored): `_routerFeeOnOutputBps` -- Tycho's cut of the swap output amount.
3. **Router fee on client fee** (stored): `_routerFeeOnClientFeeBps` -- Tycho's cut of the client fee (deducted from the
   client's portion, not from the user).

**Per-client overrides**: Both router fees can be overridden per client address via `_customRouterFees`
mapping (`CustomFees` struct, single storage slot). If set, the custom rate replaces the default for that client. Can be
removed to revert to defaults.

**Client resolution** (`_resolveClient`): When `client == address(0)` (no EIP-712 signature supplied),
`calculateFee` and `mustOutputThroughRouter` fall back to `tx.origin` for the custom fee lookup. This lets
unsigned calls still benefit from a custom rate when the originating EOA is a registered client.

**Fee scale**: Fees use 8-decimal-BPS units (1 unit = 0.0001 BPS; 100% = 100 000 000). Two public constants are
queryable via RPC:
- `MAX_BPS = 100_000_000` — 100% expressed in fee units
- `MAX_BPS_SQUARED = 10_000_000_000_000_000` — `MAX_BPS²`; the combined denominator when both fees use the
  sub-BPS scale

**Positive slippage** (`_positiveSlippageEnabled`, toggled via `setPositiveSlippageEnabled`): when enabled, the router
takes the entire surplus (`actualAmountOut - expectedAmountOut`) before fees, and the remaining fees compute on
`expectedAmountOut`. When disabled, fees compute on `actualAmountOut` and the surplus stays in the swap output. The flag
also forces `mustOutputThroughRouter` to return true, since slippage direction is unknown before the swap. Per-client
exemptions (`setPositiveSlippageExempt`, `_positiveSlippageExempt` mapping) opt a resolved client out while capture
stays enabled globally: the surplus stays in the swap output, fees compute on `actualAmountOut`, and an exempt client
with no fees skips the forced router hop.

**Deduction order**: client fee calculated first, then router's cut of client fee subtracted from it, then router fee on
output. `amountOut = amountIn - clientPortion - totalRouterFee`.

**Accounting**: FeeCalculator only computes amounts (called via staticcall). Actual distribution happens in
TychoRouterV3's `_takeFees()`, which credits fee receivers' vault balances via `_creditVault()`. `_settleOutput()` then
handles the remaining output (transfer to receiver or vault credit).

### Executors (`contracts/src/executors/`)

Each executor implements `IExecutor` (`swap` [void], `getTransferData`, `fundsExpectedAddress`). Transfer types,
receivers, and `outputToRouter` are hardcoded per-executor -- not encodable in calldata. Executors are intentionally
simple: they just call the protocol. All balance tracking, output verification, and transfer logic lives in the
Dispatcher/TransferManager.

Supported: UniswapV2, UniswapV3, UniswapV4, BalancerV2, BalancerV3, Curve, Ekubo, EkuboV3, Slipstreams, MaverickV2,
AerodromeV1, LiquidityParty, BopAMM, FermiSwap, LunarBase, RingSwapV2, Sky, Bebop (RFQ), Hashflow (RFQ),
Liquorice (RFQ), Metric (RFQ), FluidV1, Rocketpool, ERC4626, Etherfi, NativeWrap (ETH↔WETH and other native wrappers),
PropAMM (a single generic executor shared by all pAMMs implementing the standard `IPropAMM` interface; the pAMM
address travels in the swap data) and PropAMMFallback (the same liquidity routed via Titan's PropAMMRouter).

### Executor Flow, Callbacks & Output Verification

**Balance-diff verification**: The Dispatcher independently verifies every swap output. It
measures `balanceOf(measureAt, tokenOut)` before and after every `swap()` delegatecall. The measured diff becomes the
single source of truth for fees, delta accounting, and sequential chaining. This eliminates trust in protocol-reported
amounts and handles fee-on-transfer/rebasing tokens universally.

**Two output categories** (via `outputToRouter` flag from `getTransferData()`):

| Category                   | Executors                                                                                                                                       | `outputToRouter` | Behavior                                                                                         |
|----------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------|------------------|--------------------------------------------------------------------------------------------------|
| **Direct-to-receiver**     | UniswapV2, UniswapV3, UniswapV4, BalancerV2, BalancerV3, Ekubo, EkuboV3, Slipstreams, MaverickV2, AerodromeV1, LiquidityParty, ERC4626, FluidV1, BopAMM, FermiSwap, LunarBase, RingSwapV2, Sky, Metric | `false`          | Dispatcher measures balance at receiver                                                          |
| **Output-lands-at-router** | Curve, NativeWrap, Rocketpool, Etherfi, Bebop, Hashflow, Liquorice                                                                              | `true`           | Dispatcher measures at `address(this)`, then forwards via `_transferOut()` if receiver != router |

**Two input categories**:

**Direct-transfer** (UniswapV2, BalancerV2, Curve): Dispatcher staticcalls `getTransferData()` to get
the `TransferType`, receiver, tokenIn, tokenOut, and outputToRouter. Performs the transfer, then delegatecalls `swap()`.

**Callback-based** (UniswapV3, UniswapV4, BalancerV3, Ekubo, EkuboV3, Slipstreams, FluidV1, Metric): Also implement `ICallback`. Flow:

1. `getTransferData()` returns `None` (no pre-swap transfer)
2. `swap()` calls the protocol pool
3. Pool calls back to TychoRouterV3's `fallback()`
4. `fallback()` routes to `_callHandleCallbackOnExecutor()` in Dispatcher
5. Dispatcher calls `getCallbackTransferData(data, tokenIn, caller)` on the executor (a plain `view` call, not a
   delegatecall) -- returns only `(transferType, receiver)`. `tokenIn` and the amount come from the Dispatcher's own
   transient storage, so a protocol cannot inject a different token; `caller` is the `fallback()` `msg.sender`.
   **The pool's callback arguments (e.g. Uniswap V3's `amount0Delta`/`amount1Delta`) are ignored.**
6. Dispatcher performs the transfer
7. Dispatcher delegatecalls `handleCallback()` to complete the interaction

`_currentSwappingExecutor` is stored in transient storage so `fallback()` knows which executor to route to. Cleared
after the callback to prevent re-entrancy.

Transfer types returned by executors:

```
enum TransferType {
    Transfer,                 // Router sends its balance to the pool
    TransferNativeInExecutor, // ETH sent as msg.value in executor (Fluid, Rocketpool, Curve, etc.)
    ProtocolWillDebit,        // Protocol pulls from router via approval
    None                      // Callback handles it, or tokens already in place
}
```

### Transfer and Receiver Resolution

**Transfer resolution** (`_callSwapOnExecutor`): Before every swap, the Dispatcher **staticcalls** `getTransferData()`
on the current executor. Returns a hardcoded `TransferType`, receiver address, `tokenIn`, `tokenOut`,
and `outputToRouter`. `_transfer()` handles 6 scenarios based on (TransferType, isFirstSwap, isSplitSwap, isCallback).

**Output settlement** (in TychoRouterV3): After all swaps complete, `_takeFees()` deducts fees and credits fee receivers'
vault balances. Then `_settleOutput()` updates delta accounting and either credits the user's vault balance or transfers
tokens to the receiver.

**Receiver resolution** (`_sequentialSwap`): For sequential routes (A -> Pool1 -> Pool2 -> D), the Dispatcher determines
each swap's output receiver by peeking ahead and **staticcalling** `fundsExpectedAddress()` on the **next** executor.
Returns either:

- The pool address (direct-transfer protocols -- tokens go straight to pool)
- `address(this)` (callback protocols -- tokens stay in router)

Last swap's receiver is the final user/vault address.

## Rust Encoding Pipeline (`src/encoding/`)

Encodes a `Solution` into EVM calldata through three layers:

```
TychoEncoder (trait)                     -- public API, validates Solution
  └─ TychoRouterEncoder                 -- selects strategy, auto-inserts WETH swaps
       └─ strategy encoders             -- encode swap structure (single/sequential/split)
            └─ SwapEncoder (trait)       -- encodes protocol-specific pool data
```

**TychoRouterEncoder** validates each `Solution` (exact input, has swaps, no invalid cycles), auto-inserts WETH
wrap/unwrap where ETH↔WETH bridges are missing, then selects strategy: **Single** (1 swap or 1 groupable-protocol batch
with no splits), **Sequential** (multiple swaps, all `split == 0.0`), **Split** (any `split > 0.0`).

### Strategy encoders

Three concrete encoders (`evm/strategy_encoder/`), each with an `encode_strategy` method targeting a TychoRouterV3
method family. Protocol data within a
group is PLE-encoded (`[len: u16][data]...`); Ekubo uses concatenation instead (`NON_PLE_ENCODED_PROTOCOLS`).

| Strategy                        | Router methods                              | Encoding                                                                                                                           |
|---------------------------------|---------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|
| `SingleSwapStrategyEncoder`     | `singleSwap` / `Permit2` / `UsingVault`     | Groups swaps, encodes via SwapEncoder, prepends executor address                                                                   |
| `SequentialSwapStrategyEncoder` | `sequentialSwap` / `Permit2` / `UsingVault` | Validates path connectivity, groups by protocol, PLE-encodes each group with executor header                                       |
| `SplitSwapStrategyEncoder`      | `splitSwap` / `Permit2` / `UsingVault`      | Builds token array [tokenIn, intermediaries, tokenOut], encodes token indices + split percentages (U24) + executor + protocol data |

**Parallel encoding**: `encode_swap_groups` (`strategy_encoders.rs`) and `TychoRouterEncoder::encode_solutions`
spawn threads via `map_on_threads` (`evm/utils.rs`) **only when an encoder in the batch blocks on a quote** —
`SwapEncoder::blocks_on_quote()` defaults to `false` and is `true` only for the RFQ encoders (Bebop, Hashflow,
Liquorice, Metric). Otherwise encoding runs serially on the calling thread. Input order is preserved either way.

**Swap grouping** (`evm/group_swaps.rs`): Consecutive swaps on the same groupable protocol
(`GROUPABLE_PROTOCOLS` in `evm/constants.rs`: `uniswap_v4`, `uniswap_v4_hooks`, `vm:balancer_v3`,
`ekubo_v2`, `ekubo_v3`) are batched into a single `SwapGroup` and executed via one delegatecall. The `SingleSwapStrategyEncoder` can also
encode an entire multi-pool route as a single swap if all hops are on the same groupable protocol.

### SwapEncoder

**SwapEncoder trait** (`swap_encoder.rs` + `evm/swap_encoder/`): Each protocol
implements `encode_swap(&Swap, &EncodingContext) -> Result<Vec<u8>, EncodingError>`, encoding pool-specific data (pool ID, fee tiers, direction
flags) into packed bytes. Each encoder holds its executor address.

**SwapEncoderRegistry** (`swap_encoder_registry.rs`): Creates encoders by protocol system name. Reads executor addresses
from `config/executor_addresses.json`. Protocol name prefixes: `vm:` (simulation-backed,
e.g. `vm:balancer_v2`, `vm:curve`), `rfq:` (request-for-quote, e.g. `rfq:bebop`), bare (on-chain,
e.g. `uniswap_v2`, `fluid_v1`), and `pricelevelstream:` (Titan pAMM price level stream, suffixed
with the venue name or, for auto-detected pAMMs, the venue address). Price-level-stream protocols
resolve generically: a single `pricelevelstream` config entry serves the whole family via a
`get_encoder` fallback (shared generic `PropAMMSwapEncoder`/`PropAMMExecutor`), with exact
`pricelevelstream:{venue}` entries overriding per venue.

`propammfallback:{venue}` is the same liquidity executed through Titan's PropAMMRouter
(`0x4DdF368080CD7946db5b459aD591c350158175e1`, hardcoded in the executor) instead of the venue
directly, so a stale maker quote falls back to a single-hop Uniswap V3 pool rather than reverting
the route. It resolves the same
way (family key `propammfallback`, shared `PropAMMSwapEncoder`, `PropAMMFallbackExecutor`). Only venues
whitelisted on the PropAMMRouter may use the prefix.

### Angstrom attestations (`evm/swap_encoder/angstrom.rs`)

Angstrom's Uniswap V4 pools start every block locked. A swap against one carries a pool unlock attestation, signed by
the current Angstrom leader, as its `hookData`. The attestation is scoped to a block number and says nothing about the
swap, so one fetched window (covering `ANGSTROM_BLOCKS_IN_FUTURE` blocks, default 10) serves every swap, pool and route.

`AttestationCache` therefore keeps the window in a process-wide cache instead of fetching it during encoding:

- A dedicated OS thread (`angstrom-attestations`) refreshes the window every
  `ANGSTROM_ATTESTATION_REFRESH_INTERVAL`, twice per Ethereum block. Not a `tokio` task — the encoder must work without
  a runtime, and `reqwest`'s blocking client cannot be driven from inside one.
- `AttestationCache::global()` starts the thread on first call. `UniswapV4SwapEncoder::new` calls it when
  `angstrom_hook_address` is configured for the chain (`config/protocol_specific_addresses.json`), so registry
  construction warms the cache. `ANGSTROM_API_KEY` / `ANGSTROM_API_URL` / `ANGSTROM_BLOCKS_IN_FUTURE` are read once, at
  that point; without the API key the thread never starts and Angstrom swaps fail to encode with a `FatalError`.
- `encode_swap` reads the cache. A window older than `ANGSTROM_ATTESTATION_MAX_AGE` triggers one inline fetch on a
  scoped thread, so encoding degrades to the old behavior instead of failing. Fetch failures are `RecoverableError`.
- On chain, `UniswapV4Executor._selectAttestation` picks the 93-byte entry (8-byte block number + 85-byte attestation)
  matching `block.number` and returns empty bytes when none match. Entries for blocks that already passed only cost
  calldata; a window that covers no upcoming block falls back to Angstrom's protocol-driven empty-batch unlock.

### Gas estimation

`Swap::new(component, token_in: Token, token_out: Token, estimated_gas: BigUint)` carries a per-swap simulation gas
estimate (zero if unknown). Encoders aggregate these into `EncodedSolution.estimated_gas`, exposing a single estimate
for the whole solution.

## Router Trades Substreams (`substreams/`)

Standalone WASM workspace (excluded from the root workspace) indexing every trade routed through
the deployed TychoRouter contracts. Trades are recovered from EVM call traces — the router emits
no swap event — decoded per ABI generation (`v2`, `v3_0`, `v3_1`), enriched with hop/executor
data, `FeesTaken` amounts and the router fee configuration replayed from FeeCalculator events,
then emitted as `DatabaseChanges` for `substreams-sink-sql` (`schema.sql`). Per-chain manifests
live in `substreams/tycho-router-trades/chains/`.

Trades are valued in USD after ingestion, not in the substreams. Tycho prices tokens in each
chain's native token, so pricing anchors through the stablecoins pinned in
`substreams/pricing/preferred_tokens.sql` and values a trade from one trusted side, implying the
other side's price from the trade. See `substreams/README.md`.

**Adding a chain** touches six places, and the last three fail silently when missed — follow
"Adding a chain" in `substreams/README.md`:

1. `substreams/tycho-router-trades/chains/<chain>.yaml` — new manifest: `network`, the router and
   fee-calculator `params`, and `initialBlock` on all four modules.
2. `initialBlock` set from the deployment block of the earliest router on that chain (binary
   search `eth_getCode`), never a round number: the whole module graph is built from there.
3. The chain must serve Extended (Firehose) blocks; trades come from call traces, so a chain
   without them yields nothing rather than an error.
4. `substreams/pricing/preferred_tokens.sql` — the native sentinel row plus at least one pinned
   stablecoin, or the chain has no USD anchor and every trade stays unpriced. Pin by address and
   verify the implied price; symbols are duplicated by fake tokens.
5. `substreams/executors.sql` — a row per executor on the new chain, so a hop carries a protocol
   name and not only an address. Kept by hand; needs no release.
6. `substreams/docker-compose.yaml`, a released `.spkg` for the chain, and in
   `helm-configuration` the `$chains` list plus the `spkgs` pin and a `TYCHO_<CHAIN>_DATABASE_URL`
   entry in `helmwave/dev/values/tycho/router-trades/router-trades.yml`.

**The `.spkg` packages are released separately from the image** by `substreams/release.sh` to
`s3://repo.propellerheads-propellerheads/substreams/tycho-router-trades/<chain>-<version>.spkg`,
and each sink container fetches the key it is pinned to. So the image build carries no module hash
and rebuilding it is safe, while changing what a chain indexes takes three steps: bump
`tycho-router-trades/Cargo.toml`, run the **Release Router Trades Substreams** workflow, and pin
the new key under `spkgs` in helm. Releases are immutable; a rollback is a pin back.

**Changing a manifest or the Rust source changes the module hash**, which the sink cursors are
keyed by; a chain pinned to the new package then exits until its `cursors_<chain>` row is cleared,
and because the sink writes plain `INSERT`s, any chain that re-reads written blocks needs its rows
deleted too. See "Releasing a package" and "Updating a deployed sink" in `substreams/README.md`.

## Build & Test

### Solidity (Foundry)

```bash
cd contracts
forge build                     # compile
forge test -vvv                 # run all tests
forge fmt --check               # check formatting
forge fmt                       # auto-format
forge snapshot                  # gas snapshots
```

Config: `contracts/foundry.toml` -- Osaka EVM, optimizer 200 runs (default) / 1000 runs (production), via_ir enabled.
Line length 80.

Tests fork Ethereum mainnet via `RPC_URL` and Base via `BASE_RPC_URL` env vars.

Contract changes can alter the deployed runtime bytecode used by `protocol-testing`. From the
repository root, run `./protocols/testing/scripts/update_runtime_bytecode.sh` and commit any changed
`protocols/testing/fixtures/*.runtime.json`; CI runs the same script with `--check`. Foundry pins
the compiler and omits the metadata hash to keep these fixtures reproducible.

### Rust

```bash
cargo build --features evm      # build with EVM support
cargo test                      # unit tests (no fork)
cargo test --features fork-tests # integration tests (requires RPC_URL)
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Features: `evm` (default, enables alloy + reqwest), `fork-tests` (mainnet fork tests), `test-utils` (test helpers).

### CI

- **`.github/workflows/ci-foundry.yaml`**: per-project `forge fmt --check` + `forge test` + gas
  snapshot, a Slither static-analysis job, and a runtime-bytecode-fixtures freshness check
- **`.github/workflows/ci-router-trades.yaml`**: the `substreams/` router-trades workspace

## Adding a New Executor

1. Create `contracts/src/executors/NewProtocolExecutor.sol` implementing `IExecutor`
2. `swap()` returns void -- just call the protocol. No balance tracking, no output transfers, no amount validation
   needed (the Dispatcher handles all of this via balance-diff). **Never use pool return values or callback arguments to
   determine transfer amounts** -- protocols can return arbitrary data. The Dispatcher's pre/post balance diff is the
   only trusted source of truth.
3. Hardcode the correct `TransferType`, `tokenOut`, and `outputToRouter` in `getTransferData()` -- do NOT make them
   encodable. Set `outputToRouter = true` if the protocol sends output to `msg.sender` rather than accepting a receiver
   param
4. Return the correct `fundsExpectedAddress()` (pool address for direct-transfer protocols, `address(this)` for
   pull-based)
5. Add Rust encoder in `src/encoding/evm/swap_encoder/` and register in `swap_encoder_registry.rs`
6. Add integration tests in both `contracts/test/protocols/` and `tests/`
7. Add test setup in `contracts/test/TychoRouterTestSetup.sol`
8. If the executor gives the caller control over the called pool contract (e.g. the caller supplies the pool
   address), model it in the security model (`model/src/model/executors.rs`): add a variant to the `Executor` enum
   and `Executor::VARIANTS`, then implement `get_transfer_data`, `swap`, and `funds_expected_address` (plus
   `get_callback_transfer_data` and `handle_callback` for callback protocols), mirroring the Solidity executor.
   Only these caller-controlled executors are modeled — they carry the highest risk and are easiest to model.
9. Regenerate and commit runtime-bytecode fixtures with
   `./protocols/testing/scripts/update_runtime_bytecode.sh`.

## Security

### Using TychoRouterV3 (caller checklist)

When writing code that calls TychoRouterV3 swap functions:

- **Always set `expectedAmountOut` and `minAmountOut`** accurately. `expectedAmountOut` is your
  quoted output; `minAmountOut` is the revert guardrail —
  the tx reverts if the actual output falls below it. Compute it off-chain from your slippage
  tolerance. Example: 1000 USDC quoted, 5% tolerance → `expectedAmountOut = 1000 * 10**6`,
  `minAmountOut = 950 * 10**6`. The router rejects a zero `minAmountOut` and any
  `minAmountOut > expectedAmountOut`.
  Setting `minAmountOut` too low exposes the swap to MEV attacks.
- **Verify the price data** used to compute `minAmountOut` against at least one independent source.
  A `minAmountOut` derived from a bad quote may be too low to prevent a sandwiched swap.
- **Never approve infinite allowances**, including Permit2. Set Permit2 allowance and deadline as low as practical.

### Building Executors (executor checklist)

Executors run via `delegatecall` inside TychoRouterV3 — they have full access to the router's assets and storage.

- **Never call `ERC20.transfer`, `ERC20.transferFrom`, or `Permit2.transferFrom` directly.** Return transfer intent through `getTransferData`/`getCallbackTransferData`; TychoRouterV3 performs the actual transfers.
- **Never write to state variables.** Any storage write in an executor writes to TychoRouterV3's storage.
- **Do not execute `delegatecall`.** If unavoidable, ensure the caller cannot control the target address.
- **Verify callback origin.** Call `verifyCallback` inside `handleCallback` to confirm `msg.sender` is a valid pool.
- **Allowlist selectors when the caller controls calldata.** If `swap()` forwards caller-supplied calldata to an external contract (e.g. RFQ settlement), validate the first 4 bytes against an explicit allowlist of safe function selectors before making the call. An unrestricted selector lets an attacker invoke arbitrary functions on that contract — including ones that could drain TychoRouterV3's balance at the settlement contract. See `LiquoriceExecutor` for the pattern.
- `handleCallback`'s `data` argument is raw ABI-encoded calldata the executor must decode manually.
- `handleCallback`'s return value must be raw ABI-encoded data the executor encodes manually.

## Conventions

### Solidity

- Prefix private/internal state with underscore: `_feeCalculator`, `_ALLOWED_DUST`
- Transient storage slots use keccak256 of descriptive names
- Custom errors with contract-prefixed names: `TychoRouter__EmptySwaps`, `Vault__AmountZero`
- Format with `forge fmt` (80 char line length)
- Slither `// slither-disable-next-line` annotations where false positives occur

### Testing

- Foundry tests use `TychoRouterTestSetup.sol` as the shared base
- Test naming: `test_<description>` in Rust, `test<Description>` in Solidity
- **Cross-language integration tests**: Rust encoding tests
  call `write_calldata_to_file(test_identifier, hex_calldata)` (`src/encoding/evm/utils.rs`), which appends `name:hex`
  lines to `contracts/test/assets/calldata.txt`. Solidity tests then read that file
  via `loadCallDataFromFile(testName)` (`contracts/test/TestUtils.sol`) and execute the calldata against a mainnet fork.
  This verifies that Rust-encoded calldata is valid and executes correctly end-to-end.

### Git

- Submodules for Solidity dependencies (`contracts/lib/`)
- Checkout with `--recursive` to get all submodules
