# Simulation: protocol states

Rules for the protocol states in `crates/tycho-simulation`, native and VM, and for the VM adapters
under `protocols/adapter-integration/`. Off-chain venues implement the same `ProtocolSim` interface
and follow these rules too, plus `offchain.md`. The crate's structure is in
`crates/tycho-simulation/CLAUDE.md`.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds).

<!-- pieces -->

### simulation-gas-measured-on-real-path
Simulated gas reflects the protocol's real execution path: measured end-to-end for this protocol rather than copied from a similar one, quoted through the venue entry point with the same output and least overhead, with accounts a real route has already warmed (a shared vault or singleton) pre-warmed in the EVM journal rather than corrected by a tuned constant; any added constant (e.g. for caller-dependent costs) is named and documented with how it was measured and when to re-measure.
Why: Copied or adapter-only figures misprice routes; the router can pay \~2M more gas per swap than simulation reports, and inline magic numbers hide that they are estimates.
Spot: A new `const *_GAS` equal to another integration's value or an inline literal like `BigUint::from(170_000u64)`; adapter quotes through a user-facing router function instead of the cheapest equivalent view; gas figures in comments taken from a test; reliance on manifest `protocol_gas`, which nothing reads. Named constants like COLD_WARM_SLOAD_DIFF or NON_METERED_SLOT_ESTIMATE subtracted from adapter gas for a specific protocol list.
Applies: rust, rust-test, solidity, solidity-test · look first at lines matching `gas|warm|cold|sload|sstore|journal`
Checked by: repo-script `sim-gas-constant-copied`; ast-grep `sim-gas-warmth-correction-constant`

### simulation-limits-are-settleable
get_limits (natively or via the adapter's getLimits) reports the bound the venue can actually settle, derived from the on-chain constraint (field widths, reserves, only fillable levels net of fills, the inventory settlement draws from); never a placeholder 'uncapped' value, a coarse bisection over unfillable liquidity, or a silent fallback when the computation fails; exact wherever it can be derived, a conservative under-cap only with the evidence that quotes and limits agreeing requires (`simulation-quotes-and-limits-agree`), and never an overestimate, whatever the interface's doc comment suggests.
Why: The VM funds the swap with exactly the sell limit, so limits are hard: an overestimate produces fills that revert (and a failing probe can bench the pool for the session), an underestimate removes tradable size.
Spot: get_limits returning U256::MAX/u128::MAX or an `uncapped_limit()` helper; `.unwrap_or(<reserve>)`/`.unwrap_or_default()` around the limit; getLimits summing all levels ignoring dead markers, ignoring treasury/vault inventory, or quoting the whole vault balance as exact output; no comparison with brute-force ground truth.
Applies: rust, rust-test, solidity, solidity-test · look first at lines matching `limit|uncapped|inventory|vault|treasury|level|u256::max|u128::max`
Checked by: ast-grep `uncapped-get-limits`

### simulation-serde-compat-decided
Decide serde backward compatibility of ProtocolSim states explicitly: if persisted snapshots must still load, renamed fields get `#[serde(alias)]`; if not, drop `#[serde(default)]` shims for 'snapshots written before X'.
Why: Half-measures either break the persisted recordings that do depend on the format, or carry defaulted fields that mask missing data forever.
Spot: A renamed field on a Serialize/Deserialize state without alias next to other `#[serde(default)]` fields; new `#[serde(default)]` fields documented as 'defaults to zero for older snapshots'.
Applies: rust, rust-test
Checked by: ast-grep `serde-default-for-older-snapshots`

### simulation-refuse-unservable-amounts
An amount the venue rejects on chain (zero input, below a venue minimum, a zero-quantity level) is refused by the simulation with an error, never quoted as Ok(0) or as a complete fill.
Why: A swap of nothing reads as one that worked, so a route can contain a leg that reverts on chain.
Spot: get_amount_out returning Ok with a zero amount for amount_in == 0; ladder walks treating a zero amount as fully consumed; level validation accepting quantity \<= 0.
Applies: rust, rust-test, solidity, solidity-test · look first at lines matching `amount|quantit|zero|min|level|fill|==\s*0`

### simulation-caller-dependent-quotes
When a venue's price or acceptance depends on the caller (msg.sender/tx.origin screening, markups, blacklists), the adapter documents which caller its quotes assume and caller-aware quoting is tracked; quoting as the adapter is not a stand-in for the router or a solver's settlement contract.
Why: Simulation over-quotes or misses reverts for callers the venue treats differently, and none of it is visible to the solver.
Spot: Venue calls passing msg.sender through; decompiled or known screening logic; an adapter that settles as itself while execution goes through a router or solver contract.
Applies: rust, solidity, solidity-test, docs

### simulation-no-io-in-block-loop
A per-component transition applied inside the decoder's block loop does no network I/O and no writes to the shared database; anything it needs that is not in the message is recorded as pending, fetched by a background task with timeout and retries, and only that component's quotes fail until it arrives.
Why: I/O in the block loop lets one protocol's RPC round trips (unbounded without a timeout) stall every protocol's update, turns transient RPC errors into the decoder's permanent-failure policy, and makes the read-only pending-block path write shared state.
Spot: A transition, decode or apply-delta path calling get_code/eth_call/a provider, using block_in_place or Handle::current, calling init_account/insert on the shared DB, or building a provider per call; any 'first of N trait impls to do X'.
Applies: rust, rust-test · look first at lines matching `get_code|eth_call|provider|block_in_place|handle::current|init_account|insert|rpc|fetch|await|request|client|\.call\(|\bdb\b|database`
Checked by: ast-grep `io-in-transition-path`; ast-grep `io-in-protocol-or-decoder-path`; clippy `clippy::disallowed_methods (default)`

### simulation-fetch-once-check-db-first
Before fetching code or state over RPC for an account, check whether the shared database already has it, and fetch each address at most once per block.
Why: init_account ignores already-loaded accounts, so repeat fetches are discarded, add round trips, and each is another chance to hit a fatal reload error.
Spot: A get_code/RPC call before init_account with no existence check; a per-component loop fetching the same address for each component.
Applies: rust, rust-test · look first at lines matching `get_code|rpc|provider|fetch|init_account|eth_|storage|account|client`

### simulation-time-from-block-env
Time- and block-dependent values (stake limits accruing per block, amplification ramps, expanding limits, first-swap fees, freshness checks) are computed from the block environment of the block being quoted, advanced through the block hook (apply_block), never from the wall clock or from the block of the pool's last storage change; a state holds one current-block field that every method (get_amount_out, get_limits, spot_price) reads.
Why: delta_transition runs only on blocks where tracked storage moved, so time-dependent values freeze between updates, and wall-clock time makes quotes irreproducible.
Spot: SystemTime::now()/Utc::now() in ProtocolSim methods; a block_number/block_timestamp/A field updated only from delta attributes and read by pricing; a ramping parameter read once at decode with no apply_block; pending-block readings not taken under the pending block's own number and timestamp. A state struct with both latest_update_block and head_block (or similar) read by different methods; a block field that delta_transition or the block hook never advances.
Applies: rust, rust-test · look first at lines matching `block|timestamp|time|now\(|ramp|fresh|expir|stake|amplif|future_a|\ba\b`
Checked by: ast-grep `wall-clock-in-protocolsim`; ast-grep `sim-two-current-block-fields`

### simulation-prefer-native
Source: `crates/tycho-simulation/CLAUDE.md` § Simulation Approaches; `.claude/CODEBASE.md` § Key Architectural Patterns › Implementation types
Spot: A new `vm:<protocol>` integration whose contracts are a recognisable fork of a natively supported protocol; a new hybrid (native quote path) registered under a `vm:` key; a rename of an existing protocol_system bundled into an unrelated change.
Applies: rust, solidity, config, integration-test, substreams, docs · look first at lines matching `vm[:_]|protocol_system|protocol_type|native|adapter`

### simulation-one-answer-per-refusal
Each reason a swap cannot be served gets one answer used consistently by spot_price, get_amount_out and get_limits (e.g. zero amount = InvalidInput, side not quoted = no limit and no amount); refusal checks run most fundamental first (a side with no liquidity before any size-dependent minimum), so the answer names a cause the caller can act on; tests exercise each guard through every method.
Why: Overlapping refusal reasons let callers misread a broken component as an empty one, and a test that only exercises the method that already refused lets a regression in the others pass.
Spot: Methods of one ProtocolSim returning different error variants (or Ok(zero) vs Err) for the same condition; a token-pair guard test that only calls get_limits. A minimum-amount or minimum-value guard computed from the fill result before checking whether the fill consumed anything.
Applies: rust, rust-test · look first at lines matching `err|invalid|limit|spot_price|amount_out|fatal|recoverable|min|guard|zero`

### simulation-explicit-unservable-directions
A zero-limit answer for directions the venue structurally cannot serve matches those directions explicitly; any other token pair still returns a FatalError.
Why: A catch-all `_ => Ok((0, 0))` makes a token mismatch indistinguishable from an unsupported direction: the component indexes, decodes, reports every direction untradable, and the harness passes it.
Spot: A `_ =>` arm in get_limits (or a trailing return after the matched pairs) returning zero limits, especially with a comment that only justifies specific directions.
Applies: rust, rust-test · look first at lines matching `_\s*=>|limit|fatal|zero`
Checked by: ast-grep `wildcard-zero-limits`

### simulation-fallible-width-checked-decoding
Decoders read wire attributes with fallible, width-checked conversions that return the function's error type; no panicking narrowing conversion touches external data (`U256::to::<u32>()`, tycho-common's `From<Bytes>` for u8..u128 and the signed ints, `from_be_slice` on unbounded input, `Chain::id()` on a possibly custom chain).
Why: A panic bypasses the per-component error the function exists to report and takes down the decode task (skip_state_decode_failures skips errors, not panics) or the whole feed task.
Spot: `.to::<uN>()`, `as` casts, `from_be_slice`, `.unwrap()`/`.expect()` or `Chain::id()` applied to snapshot/delta attributes or API responses inside functions returning Result. `u64::from(bytes.clone())`, `u32::from(attr)`, `i128::from(value)` on attribute bytes; use `<[u8; N]>::try_from` or `try_from_be_slice` instead.
Applies: rust, rust-test · look first at lines matching `to::<|\bas\s+[iu]\d|from_be_slice|unwrap|expect|chain::id|id\(\)|::from\(|try_from|attribute|attr|bytes`
Checked by: ast-grep `panicking-narrowing-in-decoder`; clippy `clippy::unwrap_in_result`; clippy `clippy::cast_possible_truncation`; clippy `clippy::cast_sign_loss`; clippy `clippy::cast_possible_wrap`; clippy `clippy::fallible_impl_from`; clippy `clippy::indexing_slicing (review only)`

### simulation-honour-decoded-flags
Simulation honours the venue flags and parameters it already decodes (paused, recovery mode, ramping amplification), or documents why it ignores them.
Why: Ignoring a decoded pause/recovery flag or a ramping parameter quotes trades the venue rejects or prices differently.
Spot: Structs mirroring on-chain pool config (isPoolPaused, isPoolInRecoveryMode, future_A) whose fields are decoded but never read by pricing or limits.
Applies: rust, rust-test, solidity · look first at lines matching `pause|recover|ramp|future|amp|flag|enable|disable|halt|frozen|kill|active|bool|status|config`

### adapter-price-stored-when-unquotable
When no executable price exists (a price boundary with no liquidity in that direction, a venue that has stopped quoting), an adapter's `price()` at amount zero returns the pool's stored price and `getLimits` returns `[0, 0]` for that direction; it never returns a zero price, and it does not revert.
Why: A zero price gives consumers inf or a free token. A revert fails the state decode, which ends the stream unless the consumer set skip_state_decode_failures, and then drops the pool for good (failed_components is never cleared), even once liquidity returns.
Spot: price/_price returning Fraction(0, x) or 0 on a zero quote; `revert` paths in `price()` reachable at amount 0; getLimits not returning [0, 0] for a direction that cannot be filled; finite-difference code without a zero check.
Applies: solidity, solidity-test · look first at lines matching `price|limit|fraction|revert|quote`
Checked by: ast-grep `sol-price-zero-fraction`; ast-grep `sol-price-no-zero-guard`; ast-grep `sol-price-calls-swap`

### simulation-finite-difference-step
A finite-difference spot price uses a step large enough that the venue's output rounding is negligible (about 10^6 raw output units) and does not read token decimals() inside the VM.
Why: With a \~1,000-unit output floor and round-down quotes the price is biased low by up to 1/1,000, above stable-pair spreads; self-contained token proxy accounts return decimals() = 0 in the VM.
Spot: A price step derived from 10**decimals() or a small fixed floor of raw units; any decimals() call in adapter pricing code.
Applies: solidity, solidity-test, rust · look first at lines matching `decimals|price|step|10\s*\*\*|1e\d`
Checked by: ast-grep `sol-adapter-decimals-astgrep`; ast-grep `sol-adapter-decimals`

### simulation-quotes-and-limits-agree
get_limits is exactly the boundary of get_amount_out in every direction: quoting at the advertised maximum and minimum succeeds (rounding chosen so the minimum never exceeds the depth, and within the engine's per-call gas), any amount beyond the bound errors, including directions without an explicit on-chain check, and a zero limit means no amount can be traded; the agreement is pinned by a property test over random states, not only by fixed fixtures; the bound is exact wherever it can be derived. A conservative under-cap is accepted only when no exact bound can be derived, and then with evidence: why exact is not derivable, a code comment saying the cap is conservative, a test executing a trade at the cap, and the capacity it gives up quantified; for such a cap the one-unit-past check does not apply. An overestimate is never accepted.
Why: A router that asks for exactly what it was told gets an error, while a caller sizing from quotes (the testing harness's amount ladder included) gets Ok for a trade that reverts on chain.
Spot: A get_limits arm capping a direction by some state value while the matching get_amount_out path never compares amount_in to it; limits rounded up while depth is rounded down; no test calling get_amount_out at the get_limits output and one unit past it. Limit logic changes tested only on a few fixed states; gas caps in adapter view logic (e.g. _MAX_TICK_WALK_STEPS) justified by the read-only walk's gas rather than by the cost of swapping the resulting limit within the engine's 8M per-call gas. A limit described as conservative with no test at the cap or no quantified loss; a limit 'overestimated to be safe'.
Applies: rust, rust-test, solidity, solidity-test · look first at lines matching `limit|amount_out|amountout|max|min|round|steps|depth|proptest|quickcheck`

### simulation-new-state-applies-every-change
The new_state returned by get_amount_out applies every state change the on-chain swap makes, including consumed depth and balances or shares that bound later swaps, consistently across all directions.
Why: A consumer walking routes on the returned state sees the bound reset after each hop, so repeated draining trades are all accepted.
Spot: A get_amount_out branch returning `self.clone_box()` for an operation that moves tokens into or out of a contract whose balance appears in get_limits; one direction updating a field sibling directions leave untouched; a book state that never depletes.
Applies: rust, rust-test · look first at lines matching `new_state|clone|amount_out|swap|balance|reserve|deplet|state`
Checked by: ast-grep `get-amount-out-unchanged-new-state`

### simulation-mutable-conditions-in-state
Conditions that can change after a component is admitted (incentives, pauses, venue flags) are enforced in the state or adapter on every update (e.g. get_limits returns [0,0]), not by a client-side inclusion filter.
Why: Client-side filters run only on snapshots: a pool that gains the condition later stays in the stream with every quote reverting, and one filtered at startup is never readmitted.
Spot: A new client-side filter entry keyed on a mutable attribute, plus indexer work to emit that attribute only for the filter.
Applies: rust, substreams, config · look first at lines matching `filter|incentive|pause|exclu|inclu|attribute`

### simulation-shared-vm-path-scoped
A change to a shared VM/adapter call path made for one protocol is scoped to that protocol, or documented at the call site with why it is safe for every other integration.
Why: Silently changing call context (e.g. tx.origin) for all adapters can break other integrations, and readers cannot tell why the value is set on one call only.
Spot: A hunk in a generic adapter-contract or VM engine file changing a default (caller, origin, value, block override) in a PR about a single protocol.
Applies: rust, solidity
Checked by: pr-script `sim-shared-vm-change-single-protocol`

### adapter-price-function-is-marginal
An adapter advertises PriceFunction only if price(amounts) returns the marginal price after selling each amount; otherwise it drops the capability so simulation falls back to its numerical derivative.
Why: A wrong price function misprices every consumer of the spot price, while the fallback finite difference is more accurate.
Spot: price() implemented as Fraction(quote(sellAmount), sellAmount) or another ratio of a full quote.
Applies: solidity, solidity-test, config · look first at lines matching `price|capabilit|fraction`
Checked by: ast-grep `sol-price-quote-ratio`

### adapter-manifest-matches-capabilities
An adapter's manifest.yaml capabilities list matches what getCapabilities returns.
Why: Nothing enforces it, so manifest and contract drift and the manifest misdocuments the adapter.
Spot: Changes to getCapabilities or capability-dependent functions (price reverting NotImplemented) without a matching manifest edit.
Applies: solidity, solidity-test, config · look first at lines matching `capabilit|price|notimplemented|limit|manifest`
Checked by: repo-script `adapter-manifest-capabilities`

### adapter-dynamic-fee-trigger-documented
For dynamic-fee pools, the adapter comment, PR description and supported-protocols doc state exactly what triggers a fee recomputation (per block, per timestamp, per first swap) and that quotes may differ when the trade lands later.
Why: An imprecise claim misleads solvers about how stale a quoted fee can be.
Spot: Dynamic-fee adapters describing recomputation loosely; new rows in docs/for-solvers/supported-protocols.md without a fee caveat.
Applies: solidity, rust, docs, pr · look first at lines matching `fee|dynamic`

### adapter-quote-only-restriction-reverified
A quote-only adapter re-verifies the venue access restriction that made it quote-only; when settlement is callable it executes the real swap so gas is measured rather than a constant.
Why: A stale restriction claim keeps an inaccurate constant settlement gas and a weaker simulation than the venue allows.
Spot: Adapter docs claiming a gated entrypoint together with a SETTLEMENT_GAS-style constant.
Applies: solidity, solidity-test, rust, docs · look first at lines matching `gas|settl|quote|gate|restrict|access|permission|only|whitelist`
Checked by: ast-grep `sol-gas-not-measured`

### simulation-plausible-block-env
Environment values a simulated contract can read (block env fields, caller, gas) are plausible for a real block, not defaults that cannot occur on chain.
Why: Contracts that branch on environment can quote well in simulation and badly on chain.
Spot: BlockEnv/TxEnv fields left at defaults (zero coinbase, zero basefee, unrealistic gas limit) in VM simulation setup.
Applies: rust, rust-test · look first at lines matching `blockenv|txenv|block_env|tx_env|coinbase|basefee|gas|default|env|timestamp|caller|cfg`
Checked by: ast-grep `default-block-env`

### simulation-closed-form-inverse
Limits come from an exact closed form whenever one exists: the inverse of an invertible pricing function, or the venue's reference-implementation max-swap function ported (or requested upstream), not a binary search or probing loop over quotes.
Why: Binary search is slower and only approximately correct at the boundary, while the exact inverse is often already written elsewhere.
Spot: A low/high/midpoint loop calling get_amount_out-style math to find a max input for a CPMM-like curve. A get_limits that iterates quotes for a venue whose maths library exposes a max-swap function.
Applies: rust, rust-test, solidity · look first at lines matching `limit|max|binary|search|low|high|mid|bisect|loop|while`
Checked by: ast-grep `binary-search-over-amounts`

### simulation-error-variant-by-meaning
Simulation error variants are chosen by meaning. SimulationError: FatalError only for a bug in the pool or protocol that should never be simulated again, RecoverableError for transient failures, InvalidInput for bad parameters (a zero or below-minimum amount, a token not in the pair, a side nothing can fill). InvalidSnapshotError: MissingAttribute only for an absent attribute, ValueError for a present value or a setup step (e.g. building a client) that fails.
Why: A Fatal for a transient or per-trade refusal makes consumers stop simulating a healthy pool.
Spot: map_err(|e| SimulationError::FatalError(...)) around network calls or venue refusals; RecoverableError for tokens outside the pair. `InvalidSnapshotError::MissingAttribute(format!("Couldn't create ..Client: {e}"))`.
Applies: rust, rust-test · look first at lines matching `error|err\(|fatal|recoverable|invalid|missing|map_err|ok_or`
Checked by: ast-grep `fatal-error-on-network-call`; ast-grep `sim-missing-attribute-for-failed-setup`

### simulation-uniform-method-semantics
A ProtocolSim method means the same thing in every implementation (spot_price, fee, limits), its results are checked finite and positive, and no trait method is left as todo!().
Why: Routers compare implementations side by side; divergent semantics misprice multi-hop routes.
Spot: spot_price returning the mid in one implementation and the best level in another; todo!() in trait methods; unchecked non-finite results.
Applies: rust, rust-test · look first at lines matching `spot_price|fee|limit|todo!|protocolsim|finite|nan|price|amount_out`
Checked by: clippy `clippy::todo`; clippy `clippy::unimplemented`

### gas-updated-with-swap-path-work
A change that adds on-chain work to the swap path (a fresh storage slot per swap, an extra call) updates the simulated gas in the same change, priced by the storage transition it causes (zero-to-nonzero cold SSTORE about 22.1k vs about 5k for nonzero-to-nonzero).
Why: Routing compares venues on the simulated gas, so an unchanged constant makes the venue look cheaper than it is and win hops it should not.
Spot: A diff that introduces a per-swap new storage key on the venue (fresh nonce scope, new account) without touching the gas constant in get_amount_out.
Applies: rust, rust-test, solidity

### trade-limit-price-hard-floor
A trade-limit-price check treats the limit as a hard floor that no tolerance can relax; tolerance only loosens the acceptance threshold above the limit (or around a pool target price), through the shared one-sided tolerance helper.
Why: A symmetric tolerance check admits executions below the caller's limit price, which the ProtocolSim contract promises never to return.
Spot: A local `within_tolerance(a, b, tol)` using abs difference, or `execution < limit && !within_tolerance(...)` in a query_pool_swap/swap_to_price implementation.
Applies: rust, rust-test · look first at lines matching `tolerance|limit|query_pool_swap|swap_to_price|target`
Checked by: ast-grep `local-float-tolerance-helper`

### pre-swap-transitions-applied-first
State transitions the contract always applies before the swap (TWAMM virtual-order execution, first-swap-in-block observation and fee updates) are applied once to a captured start state before any equality, direction or reachability decision; every return path, including zero-amount early returns, returns that advanced state, and results are revalidated after a transition that changes the fee.
Why: Decisions made on the pre-hook state wrongly reject reachable targets or accept passed ones, and a zero swap returns a state whose spot price the chain will never show.
Spot: Early returns such as `return Ok(zero_swap(state))` or `PoolSwap::new(0, 0, self.clone_box())` placed before or without the pre-swap hook; `record_observation`/virtual-order calls after the target check.
Applies: rust, rust-test · look first at lines matching `twamm|virtual|observ|first|hook|zero_swap|poolswap|clone_box|return|fee|pre_swap|before`
Checked by: ast-grep `sim-zero-swap-unadvanced-state`

### limit-search-probe-and-bisect
A limit search distinguishes an input too small to produce output from one exceeding capacity, starts its probe from the side that binds the fill (output reserve at the price), and narrows by binary search rather than halving on any rejection.
Why: Halving on every rejection reports zero limits for tradable directions when the start probe is undersized, and reports far less than fillable depth when the probe comes from the wrong reserve.
Spot: A loop `amount >>= 1` on a quote error, or a probe derived from reserve_in for a venue whose capacity is set by reserve_out.
Applies: rust, rust-test, solidity · look first at lines matching `limit|>>=|probe|search|binary|halv|/\s*2|reserve|capacity`
Checked by: ast-grep `sim-halving-limit-search`

### adapter-runtime-regenerated
Every change to a VM adapter contract regenerates and commits the adapter runtime that tycho-simulation embeds (`crates/tycho-simulation/src/evm/protocol/vm/assets/<Name>SwapAdapter.evm.runtime`), and the diff shows a non-empty change to that asset.
Why: Simulation keeps running the previous adapter bytecode, so fixes in the adapter source never reach quoting and tests pass or fail against code that is not in the PR.
Spot: A diff touching protocols/adapter-integration/evm/src/\<protocol>/*.sol with no change to the matching .evm.runtime asset.
Applies: solidity

### walk-matches-contract-search-set
A ported liquidity walk visits exactly the set the contract's own swap visits: the entries in its search structure (bitmap/tree membership) plus any entry the contract reads before consulting that index (the current/active entry), not every entry that holds a non-zero balance; the indexer emits that membership and keeps the current entry visible.
Why: Entries the contract can never reach (dust left after the entry was removed from its search structure) get quoted and counted in get_limits, so trades sized at the limit revert on chain.
Spot: next-entry search or limit loop iterating stored per-tick/bin reserves without consulting the protocol's initialised-tick bitmap or bin tree; attributes kept for entries the contract has removed from its index. A visibility predicate defined only as 'in the tree/bitmap with reserves', while the contract's swap starts by reading the active id's reserves before any tree lookup.
Applies: rust, rust-test, substreams · look first at lines matching `tick|bin|bitmap|tree|walk|next|active|liquidity|reserve|initiali|loop|iter|level`

### router-policy-not-indexed
Component state carries no router address or router-specific caller policy; simulation assumes the router's caller treatment, so a router change needs no resync.
Why: A router change would require updating the package and resyncing every component.
Spot: A component attribute such as `quote_caller` or a whitelisted flag keyed on a specific router address.
Applies: rust, substreams, config · look first at lines matching `router|caller|whitelist|sender`
Checked by: ast-grep `substreams-router-policy-attribute`

### target-solver-monotonicity-and-bounds
A target-price solver that decides reachability from one endpoint or brackets by monotonicity establishes that monotonicity for each protocol variant (dynamic fees, off-peg multipliers and fee-inclusive prices can break it), compares against the caller's full tolerance band, and searches beyond heuristic bounds (current balance) up to the caller's max or a real protocol limit.
Why: Reachable targets are rejected as unreachable, so callers fall back to slower paths or report no liquidity where the pool can trade.
Spot: Early `TargetAboveSpot`/`TargetBelowLimit` returns computed from a single price comparison; an upper search bound set to `balances[i]`; exact-target comparisons that ignore `tolerance`.
Applies: rust, rust-test · look first at lines matching `target|tolerance|query_pool_swap|swap_to_price|monoton|bracket|bound|search`
Checked by: ast-grep `sim-target-search-bound-from-balance`

### components-simulated-independently
Each component's state is simulated independently: when several components draw on shared inventory (a shared custodian, an RFQ maker across pairs), the integration documents that limitation in its README rather than synchronizing states across components, and relies on the router's minAmountOut for the resulting reverts.
Why: Cross-state synchronization code cannot be driven by the consumer's routing (which clones and evolves states independently), adds complexity, and still cannot model the coupling correctly.
Spot: A ProtocolSim method or helper that copies balances or words from another component's state (`sync_*` from a sibling), or a static attribute added only to group components that share inventory.
Applies: rust, substreams, docs · look first at lines matching `sync|shared|sibling|inventory|custod|maker|balance|group|vault|static|attribute`

### decode-protocol-sentinels
Decoded protocol parameters are mapped through the protocol's sentinel encodings before being used as numbers (e.g. a magic fee value meaning 'explicitly zero').
Why: A sentinel read as a literal value misprices every swap on the affected pools.
Spot: Fee or parameter fields read straight from attributes into math with no match on documented magic values from the contract source.
Applies: rust, rust-test · look first at lines matching `fee|param|attr|magic|sentinel|config|0x|const`

### target-query-returns-full-limit
A price-targeting query returns the full limit when the whole tradable range already satisfies the target, rather than erroring; venues with a flat price over their range implement it natively.
Why: Erroring when the target is below the price at the limit makes the query unusable for flat-priced (oracle) pools, where that is the normal case.
Spot: A ProtocolSim delegating query_pool_swap to the generic search for a pool whose price does not move with amount.
Applies: rust, rust-test · look first at lines matching `query_pool_swap|swap_to_price|target|limit|price`

### optional-input-documented-default
An input the API declares optional (e.g. the decoder context's caller) falls back to its documented default for the chain (the deployed Tycho router), and only an unsupported chain or a malformed value errors.
Why: Requiring an optional input makes the protocol undecodable for every consumer that does not set it.
Spot: A decoder that returns an error when an `Option` context field is None.
Applies: rust, rust-test · look first at lines matching `option|none|caller|context|default|router|ok_or|some\(`
Checked by: ast-grep `sim-decoder-context-option-required`

### unknown-module-variant-rejected
A component configured with an unknown or unsupported variant of a pluggable module (fee module, hook, curve type) is rejected at decode, not simulated with a zero or default configuration.
Why: Quoting with zeroed fees for a module the simulation does not understand produces systematically wrong prices that pass every type check.
Spot: `if !is_supported(module) { return Ok(Self::default()) }` in a decoder or config builder.
Applies: rust, rust-test · look first at lines matching `support|unknown|default|module|hook|curve|variant|match|_\s*=>|kind|type`
Checked by: ast-grep `sim-unsupported-variant-default-config`

### fee-moves-against-trader
A fee applied to a price or amount always moves it against the trader in both directions (raises what a buyer pays, lowers what a seller receives), and a test pins the direction for each side.
Why: A sign-swapped fee quotes better-than-mid prices in one direction, so solvers route into trades that return less than simulated.
Spot: Price adjusted by (1 + fee) on one branch and (1 - fee) on the other where the branch conditions might be inverted (is_buy/is_sell, zero_for_one); no test asserting quote \< fee-free quote per direction.
Applies: rust, rust-test, solidity · look first at lines matching `fee`

### hook-by-permission-bits
Hook-aware pool simulation classifies a hook by its permission bits: a hook that cannot affect swaps is simulated through the generic or hookless path, not rejected as unsupported.
Why: Rejecting hooks without swap permissions drops pools whose swaps behave exactly like hookless ones.
Spot: A hook-handler factory that errors for every hook without a registered native handler, regardless of its permissions.
Applies: rust, rust-test · look first at lines matching `hook|permission|unsupported|handler`

### vm-deltas-applied-to-engine
A path that applies deltas to cloned simulation states outside the main decoder also applies the account and storage changes the VM engine reads, or explicitly rejects VM-backed protocols; it never updates only the ProtocolSim attributes of a VM state.
Why: A VM state whose attributes advance while its engine storage stays at the confirmed block quotes a mix of two blocks, with no error to show it.
Spot: New code calling delta_transition on cloned states (ephemeral, pending or what-if updates) with no update_engine/VM-storage write and no protocol filter excluding `vm:` systems.
Applies: rust, rust-test · look first at lines matching `transition|clone|pending|ephemeral|what.?if|update_engine|delta`
Checked by: ast-grep `sim-delta-transition-without-engine-update`

### simulation-amounts-compared-in-one-unit
Every amount that quoting or get_limits compares or converts is in the same unit and scale on both sides (shares vs underlying, raw atomic vs decimal-normalized), each direction's threshold is populated, and each get_limits arm is checked against its mirror direction.
Why: A conversion applied to a value already in the target unit under-reports the cap by the exchange rate, and nothing fails. A scale mismatch makes the minimum check either always pass or always fail, so the simulation quotes amounts the venue rejects.
Spot: Conversions like shares_for_amount(total_shares); asymmetric formulas between the two arms of a get_limits match; A value divided by `10^decimals` compared against a field taken unconverted from the venue API (`minimum_in_base`, `min_amount`), or a min-check that is skipped when one side's threshold defaults to 0. Conversions like shares_for_amount(total_shares) applied to a value already in the target unit; asymmetric formulas between the two arms of a get_limits match; a value divided by 10^decimals compared with a field taken unconverted from the venue API (minimum_in_base); a min-check skipped because one side's threshold defaults to 0.
Applies: rust, rust-test, solidity · look first at lines matching `share|decimal|10\^|pow|limit|min|amount|scale|unit|underlying|threshold|convert|\*\*`

### ported-math-matches-contract
Ported on-chain math reproduces the contract, not an equivalent of it: the same formula and operation order (same numerator and denominator), the same signedness (an `int` in Solidity is a signed type in Rust, never a U256 that wraps), fees applied to gross or net amounts at the same point per branch, and values the contract stores compactly quantized to that representation with its direction-aware rounding before use.
Why: Equivalent-on-paper formulas diverge under integer rounding or once a term that was zero in the fixtures becomes non-zero. A negative intermediate wraps to a huge unsigned value and silently picks the wrong branch (e.g. routing a debt-only trade through collateral), producing quotes that differ from the chain. The simulation lands on a price the contract can never store, so quotes and returned states diverge from on-chain execution at the boundary.
Spot: Simulation formulas regrouping terms differently from the contract source; tests whose fixtures make a differentiating term zero; A Rust port returning U256 for a function whose Solidity original returns int256, or whose doc comment mentions a negative case ('if a \< 0') while the type cannot hold one; subtraction in U256 without checked ops in ported routing math; A price/limit converted to a full-precision integer (Q128.128, X96) and passed straight into a protocol whose storage packs it narrower, with only a min/max range check
Applies: rust, rust-test
