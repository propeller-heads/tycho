# Indexing: Substreams packages

Rules for the Substreams packages under `protocols/substreams/` and what they emit for the indexer.
How a package is laid out, built, versioned and tested is in `protocols/substreams/CLAUDE.md`; the
Substreams helpers to use are in `libraries-tycho.md`.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds).

<!-- pieces -->

### defer-cleanup-in-syncing-package
Cosmetic cleanup (dead code with no effect on output) in a Substreams package whose spkg is already syncing waits until another change requires a new spkg, since any change forces a resync; a cleanup that changes behaviour lands in the same PR as usual (`remove-what-the-change-made-redundant`).
Why: A cosmetic-only Substreams change throws away an in-progress sync for no behavioural gain.
Spot: Requests to remove harmless dead code from a package whose spkg is already being synced or deployed.
Applies: substreams

### substreams-version-policy
Source: `protocols/substreams/CLAUDE.md` § Versioning and release
Spot: Patch bump on a PR that adds or changes emitted attributes or messages; a new `<chain>-<protocol>.yaml` whose version differs from Cargo.toml; Cargo.toml bumped without the manifests; a new package at 0.1.1+ with no earlier CHANGELOG entry. Diffs under protocols/substreams/\<pkg>/ without Cargo.toml version and CHANGELOG.md changes.
Applies: substreams
Checked by: repo-script `substreams-version-agreement`; pr-script `substreams-pr-version-bump`

### watch-repointable-addresses
Every address the protocol can re-point (vault, maker, engine, implementation, per-pool pointer slots) is watched rather than hard-coded or frozen into a component at creation: a change is either followed (fanned out to every existing component's mutable attributes, or left to DCI discovery; for a balance owner, every tracked token's balance re-read at the new owner) or pauses the affected components.
Why: Contract sets and creation-time attributes are immutable, so after a rotation simulation calls the old address, or an account the indexer never published (a MissingAccount database error the adapter's try/catch cannot catch), and the venue silently stops quoting.
Spot: An address from params/config used for contracts or `balance_owner` while the ABI has a `*Updated`/`*Set` event for it; `ChangeType::Creation` as the only write site for an address attribute; `with_contracts` including a movable address; a pool contract storing another contract's address in its own slot while only a global/config copy of that pointer is checked. balanceOf seeding triggered only by newly seen tokens while the balance owner is mutable on chain.
Applies: substreams

### balance-delta-ordinal-scale
Every `BalanceDelta.ord` is a block-scoped log/call/storage-change ordinal: a synthesized delta (e.g. an RPC-read seed) takes an ordinal from inside its transaction (its first log or storage-change ordinal), an end-of-block `balanceOf` snapshot takes one after every other delta in the block (last trace's `end_ordinal` or `u64::MAX`), never `tx.index`.
Why: `tx.index` is a different number space that sorts before or between log ordinals: a snapshot meant to supersede the block's transfer deltas is applied first and overwritten, and `aggregate_balances_changes` (which groups consecutive deltas by tx hash, later groups overwriting earlier ones) splits a transaction into two groups and drops balance changes.
Spot: `ord: tx.index` / `tx_index as u64` / `ord: 0` on a `BalanceDelta` next to deltas carrying `log.ordinal` or `change.ordinal`, followed by `sort_by_key(|d| d.ord)`; `Transaction::from(trace)` keeping only the tx when the ordinals are needed.
Applies: substreams · look first at lines matching `balancedelta|ord|ordinal|tx_index|index`
Checked by: ast-grep `balance-delta-ord-not-ordinal`

### store-graph-minimal
Keep the store graph minimal: one store per update policy where possible, no store whose only reader needs a subset another store already has, and no separate stores for values from the same source and policy.
Why: Each extra store adds a manifest entry, a module and per-block writes (e.g. a full ProtocolComponent proto per pool) without adding information.
Spot: A set_if_not_exists proto store read only for `.tokens`; two `set` string stores built from the same map with the same params; more stores than distinct update policies.
Applies: substreams · look first at lines matching `store|update_?policy|set_if_not_exists`
Checked by: repo-script `substreams-duplicate-store-signature`

### upgrade-boundary-readme
For proxy-upgradeable venues, list in the README an upgrade boundary: every external dependency (implementation, storage layouts, ABIs compiled into the adapter, helper contracts) with how the package follows it or what breaks, and for guards that rely on a specific implementation's storage layout, state that assumption and consider pausing on a code-hash change until reviewed.
Why: Upgrades have added dependencies and could move guarded slots; a guard tied to one layout passes silently after a rotation while the VM over-quotes, and nobody knows what to re-verify.
Spot: A guard that watches a fixed slot (e.g. keccak(key, base)) in a contract that can be rotated or upgraded; a fixed watched-slot list on an upgradeable pool; an upgrade-boundary table listing our own contracts or missing layout assumptions.
Applies: substreams

### absolute-balance-as-absolute
When the indexer knows a component's absolute balance (a `balanceOf` snapshot, a storage read, a reset to zero, a superseded component emptied), it emits it as an absolute value (a `BalanceChange` via `TransactionChangesBuilder::add_balance_change`, or `set` on a `set_sum` store) rather than as a computed compensating delta into an `add` store; tests cover repeated resets such as two successive supersessions.
Why: An absolute value pushed into an additive store is added to whatever is already there: re-snapshots double the balance, 'drain' deltas leave residuals when the invariant they assume breaks, and nothing fails loudly — quotes price against the wrong reserve.
Spot: A `BalanceDelta` whose `delta` is `balanceOf(...)` or `-balanceOf(...)`; `updatePolicy: add` on a store that also receives snapshots; long comments justifying why a negative delta should net an additive store to zero; code computing `new - old` at the call site. A drain/negating delta emitted in a loop over all superseded or older components for a key; tests covering only a single supersession.
Applies: substreams · look first at lines matching `balance|set_sum|add|delta|update_?policy`
Checked by: ast-grep `balanceof-snapshot-as-delta`

### seed-balance-once
Each component's balance is seeded exactly once, at creation, from state consistent with the block it is anchored to: not missing (starting at zero), not added on top of the same block's derived deltas, and not multiplied when several components are created or superseded in one block.
Why: Balance stores are additive, so a seed gap or an overlap is a permanent offset for the life of the integration, and the next person who reruns the snapshot script at a busier start block gets a silently corrupt seed.
Spot: Seed deltas emitted in `start_block` while the delta-deriving branch is also active in that block; snapshot scripts reading state `--block $START_BLOCK` (post-block) rather than `START_BLOCK - 1`; new/replacement components with no seeding path; per-iteration `+=` of a shared balance inside a loop over same-block events.
Applies: substreams, script · look first at lines matching `seed|balance|start_block|snapshot|block|creat|component|\+=`

### derive-balance-from-protocol-accounting
Where the protocol's own accounting variables determine a balance or liquidity figure exactly, derive it from those (already-indexed) values instead of reconstructing it by accumulating decoded calls or transfers.
Why: Accumulation misses every movement the decoded paths don't cover (donations, new entrypoints after an upgrade, rebases that emit nothing) and drifts silently; derived values cannot drift and need no seed.
Spot: A balance built as `seed + Σwrap − Σunwrap` from decoded function calls; a relative-balance pipeline (`map_relative_balances` + `store_balances`) for values that are already read absolutely from storage; a rebasing token's balance tracked from Transfer events.
Applies: substreams · look first at lines matching `balance|liquidity|reserve|supply|delta|wrap|transfer|rebase|share`

### same-block-lifecycle
Lifecycle handling is correct when the same entity sees several lifecycle events in one block (add/remove/re-add, several registrations of the same asset), and per-entity state changes compose with venue-wide state (a per-component unpause does not override a global pause).
Why: Same-block sequences emit duplicate components or multiply balances, and an un-composed pause flag can mark a component active while the whole venue is paused.
Spot: Per-event emission keyed on 'first seen in this block' sets without dedupe across the whole block; `change_component_pause_state(id, false)` on re-registration without checking a global paused flag.
Applies: substreams

### slots-as-code-constants
Values fixed by compiled contract code (storage slots, mapping base slots, packing offsets) are named constants in code, documented where decoded; only values that differ per deployment (addresses) are manifest params.
Why: A param can move a slot but not change its decoding, applies retroactively to the whole indexed history, permits overlapping values that code silently treats as distinct, and makes doc comments restating the value go stale.
Spot: Slot numbers or mapping bases in `params:` / DeploymentConfig next to hard-coded slots from the same contract; doc comments like 'Pair slot 48' while the value lives in the yaml; if-chains over param slots that could coincide.
Applies: substreams · look first at lines matching `slot|param|offset|base|config|const`

### upgradeable-slots-validated-at-tip
Storage slots and formulas for upgradeable contracts are validated against the implementation live behind the proxy at both the package's start block and the chain tip, not against a cited or historical implementation or a test range that predates the latest upgrade.
Why: Proxy upgrades relocate, wipe or deprecate slots; an indexer built on the old layout keeps emitting zeros or gap contents that read as valid state, and CI stays green when its test range predates the upgrade.
Spot: Slot constants or comments citing an implementation address without the block it was live; no check of the EIP-1967 implementation slot at start_block and head; a test range far behind tip for an upgradeable contract; variables whose getter reverts on the live implementation.
Applies: substreams, integration-test

### balances-from-transfer-events
A component's balance follows everything that changes the holder's usable balance, not one convenient signal. Where the protocol's own accounting or an absolute read gives the balance exactly, use it (`derive-balance-from-protocol-accounting`). Otherwise track deltas from the token's own events (ERC-20 `Transfer`, plus WETH `Deposit(dst)` and `Withdrawal(src)`, which mint and burn without a Transfer log, through tycho_substreams::balances::extract_balance_deltas_from_tx), with self-transfers netting to zero, never from decoded function calls or selected entrypoints. Either way, list what can move the balance without passing through that source (rebases, donations, external shares, custody rotation, eventless configuration writes) and handle each or document it as a known limitation.
Why: Decoded calls cover only the entrypoints someone listed, and events miss rebases, share changes and eventless writes; whatever bypasses the source moves the balance unseen, so the indexed balance drifts permanently.
Spot: `Wrap::match_and_decode(call)`/`Deposit` call decoding to compute token deltas; hand-rolled Transfer loops with `if from == x {..} else if to == x {..}` that debit on a self-transfer; a balance-delta module matching only `Transfer` logs for a holder that can hold WETH, with no Deposit/Withdrawal arm and no use of tycho_substreams::balances; a rebasing or share-based token tracked from Transfer events; a custody holder that can change with no handling; no README or code note on what bypasses the balance source.
Applies: substreams · look first at lines matching `transfer|deposit|withdraw|balance|wrap|match_and_decode|delta`
Checked by: ast-grep `substreams-hand-rolled-transfer-deltas`

### sequential-id-discovery
When a protocol keys components by a sequential on-chain ID, discover or enumerate them up to the first unused ID (probing the next ID from the highest known one); a fixed bound is allowed only when it covers the whole on-chain ID range (the ID's type, or a cap the contract enforces). This holds for the Substreams package and for the VM adapter's pool enumeration.
Why: A bound below the on-chain range silently stops discovering components once the venue passes it, and nothing fails; walking a large range costs work (hashing per block, calls on empty slots) where nothing changed.
Spot: A `MAX_*` constant below the ID's on-chain range bounding a `(0..MAX)` loop that builds slot/ID lookup tables in a map handler, or bounding getPoolIds/discovery in an adapter; adapter loops that skip zero entries instead of breaking at the first unused ID.
Applies: substreams, solidity
Checked by: ast-grep `fixed-id-range-enumeration`; ast-grep `sol-fixed-range-id-scan`

### one-time-event-config-fallback
State that the package learns from a one-time event (initialisation, configuration) has a configured fallback, so indexing from a later start block or a shortened test range produces the same output as a full sync; when both the event and the configured value are missing, the module fails loudly.
Why: If the event is outside the indexed range, the module returns early and emits nothing, and the venue looks exactly like one that never traded — with no diagnostic anyone sees.
Spot: `store.get_last(KEY)` returning `None` causing an early `return`/`Ok(empty)`; the same value present in params but not used as `store.get_last(KEY).or(config.x)`.
Applies: substreams · look first at lines matching `get_last|get_first|store|config|param|none|init|option|or\(`
Checked by: ast-grep `one-time-store-value-no-fallback`

### params-all-consumed
Each Params struct field is needed by the module, and generic hooks (e.g. a configurable protocol type name) are exposed only when the same WASM is actually reused by multiple protocols.
Why: `serde_qs` silently ignores unknown keys, so a stale param reads as live configuration, and an unnecessary knob invites misconfiguration.
Spot: A key in `params:` of the manifest with no matching `Config`/`Params` field; a `protocol_type_name` param in a package that serves a single protocol.
Applies: substreams · look first at lines matching `param|config|serde|protocol_type_name|pub |struct`
Checked by: repo-script `substreams-params-keys-consumed`

### registry-repoint-pauses-old
When a registry/factory write re-points a key that previously resolved to a known component, the old component is paused (compare the write's old_value with known components), and the adapter/executor verifies on every call that the registry still resolves to the component's own pool id.
Why: An old component that keeps resolving by token pair prices the new pool's liquidity; solvers see the same depth twice and split routes revert or fill badly.
Spot: Discovery on registry writes that only adds new components; handlers that treat a registry write only as 'refresh everything'; adapters/executors that look up the pool by (tokenIn, tokenOut) through a router and never compare with the component id.
Applies: substreams, solidity

### no-zero-address-dependency
Do not emit a dependency/contract attribute with a zero address; keep the guard that prevents it, with a comment explaining the consumer-side consequence.
Why: A published zero address makes the consumer eth_getCode(0x0), whose empty answer is a FatalError for the whole stream.
Spot: Emission of stateless_contract_addr_* or similar address attributes without a non-zero check; removal of such a check as a 'simplification'.
Applies: substreams · look first at lines matching `stateless_contract|address|addr|zero|is_zero|0x0|contract|dependenc`

### state-deployment-assumptions
Discovery rules that depend on a deployment pattern (all init writes and the registry entry in one transaction) are stated as deployment assumptions in a comment, with what they cost (pools that don't match are never discovered).
Why: Without that, a future deployment that initializes and registers in separate transactions is silently never indexed, and nobody knows the rule needs rechecking.
Spot: Discovery that requires several writes in the same transaction; a comment that states the rule but not its consequence.
Applies: substreams

### substreams-log-not-diagnostic
`substreams::log` output is not a diagnostic: production streams strip module logs and native builds print nothing, so a condition that matters must surface in the emitted data, as a value that keeps the entity inert (a closed or skipped state; a zero counts only where the consumer reads it as one), or, when no such value exists, panic.
Why: A log-and-skip path reaches nobody, so a misconfiguration that disables all output is invisible in production and in `cargo test`.
Spot: `substreams::log::info!/warn!` followed by `return`/`continue` that skips emitting components or balances.
Applies: substreams · look first at lines matching `log::|log!|info!|warn!|debug!|error!|substreams::log`
Checked by: ast-grep `substreams-log-then-skip`

### filter-unregistered-entity-events
Event filters only pass events for entities whose registration has been observed, and any filter that exists only to make partial-range syncs work says so in a module comment.
Why: Events for unregistered entities create attribute/state updates for components that don't exist, and an unexplained filter looks like load-bearing logic to the next maintainer.
Spot: Event variants unconditionally accepted (`=> true`) in a match where sibling variants require a store lookup of the registered entity.
Applies: substreams · look first at lines matching `true|filter|match|=>|registered|get_last`

### module-prefix-matches-graph
Module source files carry numeric prefixes that follow the module graph stage (all modules at the same depth share a prefix, the final `map_protocol_changes` is last).
Why: The prefixes are how readers navigate the pipeline; prefixes out of step with the graph mislead about data flow.
Spot: New modules added to `src/modules/` whose `N_` prefix doesn't match their position in the manifest's input graph.
Applies: substreams
Checked by: repo-script `substreams-module-prefix-depth`

### published-tycho-substreams-has-fixes
A Substreams package depends on a released tycho-substreams/substreams-helper (crates.io version or git tag), never a path dependency on the in-tree crate, and the pinned release contains every fix the package relies on: when the fix exists only in the in-tree crate, publish (or tag) first.
Why: Two divergent crates with the same version number in one workspace silently ship old behaviour — here, contract changes with only token balances classified empty and dropped, the exact drift the PR meant to fix.
Spot: `tycho-substreams = "x.y.z"` bumps in a package Cargo.toml while protocols/substreams/crates/tycho-substreams has unreleased commits at the same version. `tycho-substreams = { path = "../crates/tycho-substreams" }` in a protocols/substreams/*/Cargo.toml; a version bump while the in-tree crate has unreleased commits at the same version.
Applies: substreams, cargo · look first at lines matching `tycho-substreams|substreams-helper|tycho_substreams|path|version|git|tag`
Checked by: repo-script `tycho-substreams-unreleased-at-pinned-version`

### prune-unbounded-storage
Component state carries only storage the simulation reads: a package indexing a contract through storage diffs forwards only the slots or slot ranges the simulation decodes, and for unbounded structures (an oracle ring buffer) emits only entries reachable by simulation and deletes the rest with `ChangeType::Deletion`, keeping a small per-pool store to choose them.
Why: Emitting every ring write ships megabytes per busy pool in each snapshot, slowing sync and clients.
Spot: Substreams emitting every storage slot of an array/ring (observations/{idx}) with no deletions. A storage-change filter keyed only on the contract address with no slot predicate; emission of every slot of an array/ring with no deletions.
Applies: substreams · look first at lines matching `storage|slot|deletion|change|filter|address|observation|contract`

### enumerate-swap-entrypoints
When integrating a protocol, enumerate every swap-like entrypoint the contracts expose (including `receive()` shortcuts and one-directional wraps) and decide explicitly whether each is a component.
Why: Missing entrypoints leave reachable liquidity/routes unindexed.
Spot: Integration PRs whose components don't cover payable `receive()`/fallback swap shortcuts or combined stake-and-wrap paths present in the contract.
Applies: substreams, solidity

### substreams-package-layout
Source: `protocols/substreams/CLAUDE.md` § Layout; `protocols/substreams/CLAUDE.md` § Adding a new protocol
Spot: New packages without these files/entries; rust-toolchain.toml set to stable.
Applies: substreams, cargo, config
Checked by: repo-script `substreams-package-layout` (full)

### finalized-only-versioned-writes
Source: `crates/tycho-storage/CLAUDE.md` § Write Order; `crates/tycho-storage/CLAUDE.md` § Temporal Model; `.claude/CODEBASE.md` § Key Architectural Patterns › ReorgBuffer + finality
Spot: DB writes outside the ReorgBuffer drain path; UPDATEs of valid rows' data; new WriteOp variants not placed in DBCacheWriteExecutor's order.
Applies: rust, rust-test, sql · look first at lines matching `versioning|write|update|insert|flush|WriteOp|ReorgBuffer|commit|upsert|delete|db|gateway`

### initial-block-is-deployment
A Substreams manifest's initialBlock is the deployment block of the earliest contract it indexes (found by binary-searching eth_getCode), never a round number.
Why: The whole module graph is built from initialBlock; too early wastes sync, too late misses state.
Spot: initialBlock values ending in many zeros or chosen without a deployment lookup.
Applies: substreams · look first at lines matching `initialBlock|initial_block|start_block|startBlock`
Checked by: repo-script `substreams-round-initial-block`

### tokens-by-address
Tokens are pinned by address, never by symbol.
Why: Symbols are duplicated by fake tokens, so a symbol lookup can price or route the wrong asset.
Spot: Config, SQL or code matching tokens by symbol string.
Applies: substreams, rust, sql, config, integration-test, script · look first at lines matching `symbol|usdc|weth|usdt|dai|wbtc|eth|token`
Checked by: ast-grep `token-matched-by-symbol`

### delta-window-bound
Source: `crates/tycho-indexer/CLAUDE.md` § Why extractor messages must be broadcast to the RPC service
Spot: New RPC/state code combining gateway reads with DeltaWindow contents without a db_committed bound.
Applies: rust, rust-test · look first at lines matching `window|delta|db_committed|committed|gateway|merge`

### trace-runtime-branch
An entrypoint traced once per component must be traced on the code path that runtime takes; time- or state-dependent branches need a trigger or retrace that covers the other branch.
Why: A one-shot trace on the wrong branch misses the storage the simulation later needs.
Spot: Substreams emitting entrypoints only on first interaction for contracts with block.timestamp-dependent branches.
Applies: substreams · look first at lines matching `entrypoint|entry_point|trace|dci|timestamp|retrace`

### token-analysis-pool-transfer-path
Token behaviour analysis exercises the transfer path a swap takes (the pool as sender and recipient, per pool), not a shared holder whose transfers the token never taxes.
Why: Pair-only transfer taxes stay invisible, so quality-100 tokens make swaps revert on K or fill short.
Spot: Token analyzer changes that pick a fixed holder for all pools, or store one verdict per token for a tax that varies by counterparty.
Applies: rust, rust-test · look first at lines matching `analy|holder|tax|fee|transfer|quality|token`

### static-only-if-immutable
A value is a static attribute only when the contract makes it immutable (confirmed from the contract code); anything held in mutable storage or settable by an admin (interface addresses, fee terms) is a state attribute from creation, initialised by its update event when the protocol emits one in the creation transaction, and is never emitted static and later moved into state.
Why: A static attribute cannot follow an on-chain change, so the component simulates with the old value until a resync, and a value moved from static to state leaves two code paths consumers must reconcile.
Spot: `with_attributes`/static_att for an address or parameter that has a setter on chain; a change that removes an entry from static_att and re-emits it as a state change; the same name in both static and state attributes.
Applies: substreams · look first at lines matching `static|with_attributes|attribute|attr`

### vm-balance-is-real-balanceof
For a VM-simulated component, the indexed balance of each token equals the holder's real ERC20 `balanceOf` (uncollected protocol fees, donations and dust included) whenever the contract reads `balanceOf` during a swap; protocol-internal accounting such as 'tradable TVL' is the source only when the contract prices from that cached value.
Why: The VM overwrites the token's balance with the indexed component balance, so a balance net of retained fees makes the contract's own input/inventory validation fail ('insufficient funds') or price from the wrong reserves.
Spot: A substreams package that computes balances from the pool's internal reserve/accounting getters or events while the adapter/contract calls `IERC20(token).balanceOf(pool)`; a README or comment saying the package reports 'tradable' or 'net of fees' balances.
Applies: substreams · look first at lines matching `balance|reserve|tvl|fee|accounting|tradable`

### vm-swap-path-contracts-attached
Every contract a VM adapter's swap path touches (helper/store contracts, delegatecall implementation libraries, price helpers) is attached to the component's contracts or reached through DCI, and the integration test exercises a swap that loads each of them.
Why: The production engine database has no RPC fallback, so an unindexed account makes every quote fail with MissingAccount.
Spot: A new adapter or contract upgrade that adds calls to another address (a store, an ExtraImpl library, a helper the adapter constructor takes) without a matching entry in the component's `contracts` or a DCI entrypoint.
Applies: substreams, solidity, integration-test · look first at lines matching `contract|address|entrypoint|dci|call|library|delegatecall|helper|store`

### modules-survive-external-values
A Substreams module never returns Err, panics or asserts on a condition that the indexed contract or a permissionless on-chain action can produce (an unusually large jump, an unseen id, a fee set past its typical range, a missing companion write); it emits the value or skips the entity, leaves validation to simulation, and reserves fatal errors for violated invariants of the package's own data.
Why: A module error halts the extractor at that block for every component on it, so anyone who can create a pool, or one governance change to a fee, stops indexing of the whole protocol.
Spot: A cap, `ensure!`, `assert!`, `expect` or `missing_is_fatal` in a map module on values derived from user-controlled events, governance-set parameters or pool state (`if span > MAX { return Err(..) }`, `bps <= 1000`, `log.topics.len() == 3`).
Applies: substreams · look first at lines matching `err|panic|assert|expect|ensure|unwrap|unreachable|fatal|bail|todo!|unimplemented`
Checked by: ast-grep `substreams-fatal-on-value-bound`

### required-attributes-emitted-at-creation
Every state attribute the simulation decoder requires is emitted when the component is created, with its initial on-chain value, and is not left to be emitted only by the event that later changes it.
Why: A component whose configuring event never fired (or fired before the start block) lacks a required attribute, so the decoder rejects it and the pool is never simulated.
Spot: Decoder uses require_*/get_* (hard failure) on an attribute that the Substreams package only writes from an event handler (fee set, pause, config update), with no Creation-time default in the component-creation path. A new package for a fork of an integrated venue that renames or conditionally emits an attribute the shared decoder checks with `.get(..).is_some()`.
Applies: substreams, rust · look first at lines matching `attribute|attr|require_|creation|get\(|is_some|hook`

### no-per-block-rpc-in-substreams
Substreams packages derive state from decoded events and storage diffs; RPC calls from inside modules are limited to one-time component-creation metadata and are batched (RpcBatch), never made per block to read live state.
Why: Per-block eth_calls from Substreams slow indexing for the whole chain and make the output depend on an RPC endpoint.
Spot: A map module calling a view function (getQuoteState, getReserves) via substreams_ethereum::rpc on every block or every swap; several sequential RPC calls at creation that could be one batch.
Applies: substreams · look first at lines matching `rpc|eth_call|batch|call|view`
Checked by: ast-grep `substreams-module-rpc-call`

### known-component-check-reads-store
The known-component check reads the component store (get_last on the component key), not manifest params or a scan of the current block's new-component output; the store already reflects components created earlier in the same block.
Why: A params-based check duplicates the registry, breaks once components are discovered from a factory, and a separate scan path drifts from the store.
Spot: A changes module taking both params and the component store and checking membership against the params pool list, or iterating tx_components to see whether a component is new.
Applies: substreams · look first at lines matching `store|get_last|param|component|tx_components|new`

### first-emission-is-creation
An attribute emitted for the first time for a component uses `ChangeType::Creation`, and `ChangeType::Update` only for later changes to an attribute that already exists.
Why: The indexer uses the creation marker on reorgs to decide whether a reverted attribute is deleted or restored to a prior value, so a creation sent as Update is misclassified on revert.
Spot: A shared `attribute(name, value)` helper hardcoding `ChangeType::Update` that is also used for the component's initial attributes.
Applies: substreams · look first at lines matching `changetype|creation|update|attribute`
Checked by: ast-grep `substreams-attribute-helper-hardcoded-update`

### balance-counts-only-payable-inventory
A natively simulated component's balance is what that component alone can pay out: when payout is bounded by more than one source (its own reserve and a shared backing, a common vault), the balance is the binding minimum, which is the full shared amount on every component the shared source binds. VM components follow the real balanceOf (`vm-balance-is-real-balanceof`).
Why: A balance above what the component can pay out inflates its TVL, so TVL filters admit pools that cannot fill; a balance below it (a shared amount split by a ratio) hides size the component can trade.
Spot: A component balance taken from the balance of a contract that serves several components (wrapper, vault, singleton) without bounding it by the component's own reserve; a shared balance divided among components by a share or ratio that does not bound payout.
Applies: substreams · look first at lines matching `balance|reserve|backing|vault|wrapper|tvl`

### discover-via-protocol-events
A Substreams package finds the transactions it cares about through the protocol's own events or calls (Swap/Mint/Burn, calls to its contracts) and decodes token Transfer logs only where the balance model needs them, not by scanning every ERC20 Transfer in every block to discover activity.
Why: Decoding and store-looking-up every Transfer log on the chain adds large indexing latency to every block and slows full syncs by days.
Spot: A `map_*` module whose discovery or filtering loop starts from `Transfer::match_and_decode` over `block.logs()` with no prior gate on the protocol's addresses or events.
Applies: substreams · look first at lines matching `transfer|logs|match_and_decode|filter|address|event`
Checked by: ast-grep `substreams-transfer-scan-over-all-logs`

### refresh-covers-every-writer
When a package refreshes a storage value on specific events, it lists every code path that writes that variable and the event each emits; a writer without an event means watching the slot directly, and an implementation upgrade pauses the component until the list is re-checked.
Why: A writer that the chosen events miss leaves the indexed value stale after that path runs, with nothing to signal it.
Spot: Event-gated storage reads (`if topic == X || topic == Y { slots.insert(...) }`) with no comment on why those events cover every writer.
Applies: substreams · look first at lines matching `topic|event|slot|refresh|insert|storage|match_and_decode`

### filters-mirror-contract-acceptance
Indexer filters that drop events or calls replicate the contract's actual acceptance rules (its configured windows and checks), not a stricter invariant assumed from typical behaviour.
Why: A stricter filter silently skips updates the contract accepted, so indexed state diverges from on-chain state whenever the venue uses the slack it allows.
Spot: An early `return None` on a field mismatch (timestamp != block timestamp, target != expected address) without a comment citing the contract check it mirrors.
Applies: substreams · look first at lines matching `return|filter|timestamp|!=|==|continue|skip`

### store-values-bounded
Values in a Substreams store are bounded in size (the reviewer cites about 8 KB per value), so a list under one key that grows with users, takers or pairs is sharded by key or redesigned.
Why: An append store that outgrows the value limit fails the module once enough entries accumulate, long after the package shipped.
Spot: `store.append(ord, "<fixed key>", ...)` where the number of appended items is driven by external actors (whitelisted takers, listed pairs).
Applies: substreams · look first at lines matching `append|store|set|key|format!`
Checked by: ast-grep `substreams-store-append-unbounded`

### fork-gets-own-package
A change that adds a new kind of component to a live package (a fork of another protocol, a new extension or hook of the same one) and needs decoding, state or execution steps the package does not yet have gets its own package and protocol system, built on the original's modules (a copy for a fork, the original crate as an rlib for an extension), whenever keeping it inside would change the output for, or force a resync of, components the change does not touch. The exception is a live package that already emits those components whatever their kind: splitting them out then needs a module change to the live package too (to stop emitting them), which resyncs it anyway, and their ids already belong to it (component ids are unique per chain across protocol systems), so the new kind stays inside.
Why: Changing the original package forces a resync of every component it already indexes and couples two venues' release cycles.
Spot: A protocol-integration PR editing another protocol's live Substreams package or encoder to add a branch for the new venue; a PR adding an extension or hook to a live package with a version bump, a new store, or new creation-time attributes, while the components it targets are few or new. An extension being "part of the same protocol" is not a reason to keep it inside: the test is which components must resync. Before asking for a split, check whether the live package already creates the components the change targets.
Applies: substreams, rust, solidity

### distinct-component-tokens
A component's token list contains distinct addresses, and discovery rejects components whose tokens collapse to the same address (e.g. two wrappers of the same underlying).
Why: Duplicate tokens violate the protocol_component_holds_token primary key and stop the extractor during sync.
Spot: Component tokens derived through a mapping (underlying of a wrapper, rebasing token's base) without a token0 != token1 check.
Applies: substreams · look first at lines matching `token|underlying|wrapper`

### range-revert-restores-pre-range-value
A revert of a block range restores each (component, attribute) to its value before the range, decided by the first event for that key inside the range: if that first event is a creation the attribute did not exist and reverts to absent, otherwise its prior value is restored; an attribute created and deleted inside the range emits nothing.
Why: A pre-existing tick that is deleted and re-created inside the reverted range is erased from consumers' state instead of restored, and a create-then-delete emits a spurious second deletion.
Spot: Revert code that collects every key seen in any created_attributes set of the range, or applies two passes (created set, then lookups) instead of one chronological pass.
Applies: rust, rust-test · look first at lines matching `revert|reorg|creat|delet|attribute|range|rollback`

### lww-guard-uses-db-order
A last-writer-wins guard in an indexer cache orders writes with the same total order the database uses for versions (block timestamp plus a tiebreak for blocks sharing a timestamp).
Why: On fast chains consecutive blocks share a timestamp, so one extractor's older block overwrites another's newer value, and a 0 placeholder is indistinguishable from a real block number.
Spot: A cache write guarded by `ts >= stored_ts` alone, or an ordering key built with block_number: 0 for snapshot rows.
Applies: rust, rust-test · look first at lines matching `ts|timestamp|>=|block_number|order|version|cache|write`
Checked by: ast-grep `indexer-version-order-guard`

### per-block-work-minimal
Substreams handlers run on every block, so per-block work is only what the block's relevant changes need: values used in a rarely taken branch are computed lazily on first use (LazyCell), hot gates filter before they collect or sort (a 'does this tx touch X' check iterates calls directly), data already in ordinal order (`logs_with_calls()`, appends in iteration order) is not re-sorted, and a module that needs what changed in a store takes the store's deltas instead of reading the store at two ordinals or re-scanning every key per transaction.
Why: Substreams handlers run on every block; eager enumeration or host-boundary store reads whose result is discarded in most blocks are pure overhead across the whole sync. `logs_with_calls()` and 'collect every storage change then sort' allocate and sort tens of thousands of items per busy block only to discard nearly all of them, on every block of the sync. A redundant sort costs work every block and suggests to the reader that the input order is not trustworthy, hiding which ordering is actually load-bearing. Double reads and per-key scans multiply store access by the number of keys and transactions in every block.
Spot: `let x = enumerate_*(store)` / `block_start_*_state(...)` / `or_insert_with(TransactionChangesBuilder::new)` for every tx at the top of a function whose use sits inside a rarely-taken branch; `LazyCell` absent where a value is used conditionally; `logs_with_calls()` used just to test `any()`/first element; `ordered_storage_changes`-style helpers that collect and sort all changes before the caller filters to a few tracked slots; the same full sort run twice per tx; `sort_unstable_by_key(|e| e.log_ordinal)` / `sort_by_key(|d| d.ord)` after building a Vec from `logs_with_calls()` or from a single in-order pass; `get_at(0, k)` and `get_last(k)` / `get_at(u64::MAX, k)` on the same key to detect creation, or a loop over all known bases per tx reading the store. Keep each absorbed detect: eager enumerate_*/block_start_* at the top of a handler; `logs_with_calls()` used only for any()/first; sort_by_key on ordinals after an in-order build; get_at(0, k) plus get_last(k) on the same key.
Applies: substreams
Checked by: ast-grep `logs-with-calls-as-gate`; ast-grep `redundant-ordinal-sort`; ast-grep `substreams-store-read-at-two-ordinals`

### attribute-by-emitting-contract
Indexed data is attributed by the contract it comes from before anything in it is trusted: storage-change matching and filtering key on (contract address, slot), never the slot alone (an address-unrestricted slot filter is justified in the README), and event decoding checks log.address against the known singleton, factory or registered pool before using any id or value in the event.
Why: Slot numbers collide across contracts: another contract's write to the same slot is decoded as the tracked attribute (mispricing until the next genuine write), and common layouts such as mapping entry 0 at slot 1 are written by busy unrelated contracts every block, flooding the filter and stores. Any contract can emit an event with the same signature and a real pool id, letting an attacker set a pool's price/fees in the index or hit a fatal branch that halts the extractor.
Spot: `tracked_slot`/`get_changed_attributes` or `w.key == some_slot` comparisons without an address check, fed changes from several contracts; tracked-slot tables with no address column; `fee:<address>`-style entries stored for every address on chain; match_and_decode on a log followed by a lookup by an indexed id (pool id, book id) with no comparison of log.address against a param or store; test logs built with an empty address. Keep both detects: slot comparisons without an address check over changes from several contracts; match_and_decode followed by a lookup by an indexed id with no log.address comparison, or test logs built with an empty address.
Applies: substreams · look first at lines matching `slot|address|match_and_decode|storage|log|key|tracked|fee`
Checked by: ast-grep `storage-change-slot-only-match`; ast-grep `substreams-event-decoded-without-address-check`

### component-id-matches-pool-identity
A component id identifies exactly one pool, chain-wide and across protocols: an id that is not a contract address includes deployment-specific input (the venue or router address), and an id or key derived from a token pair uses the orientation and uniqueness the contract itself confirms ((A,B) and (B,A) are one key only when the contract treats them as one pool).
Why: ID collisions between protocols or deployments are silently discarded by the indexer, so a second venue with the same derivation never gets its components. A wrong uniqueness or orientation assumption merges distinct pools into one component or emits two components for one pool.
Spot: `component_id` derived only from a token pair (`keccak(token0, token1)`) or a bare numeric index; A component id or store key derived from (token0, token1), especially one written under both orderings
Applies: substreams
Checked by: ast-grep `component-id-from-token-pair`

### tradability-as-pause-state
Whether a component can trade is mutable pause state, handled through one model: the component is marked by the `paused` attribute ([1u8] to pause, deleted to unpause, both through `change_component_pause_state`), never by zeroed balances or TVL and never by a static attribute; a component the venue creates disabled is emitted paused at creation and unpaused by the enabling event; a pause whose condition is fully observable on chain is lifted in the transaction that clears it, and only pauses needing human re-verification stay permanent; and every pause or kill switch the venue has, global or per pool, is either indexed this way or named in the README with why it is not handled.
Why: Zeroed balances corrupt TVL and the VM's balance overwrites while the component stays in the graph; Tycho cannot delete a protocol component, so the only off-switch is state the consumer reads. Writing [0u8] for 'active' leaves a present `paused` attribute, and a component created before activation is quotable while every swap reverts. A one-block false positive or a later fix removes the component for good, and recovery needs a new spkg and a re-sync. An unindexed pause leaves components quotable while every swap reverts on chain.
Spot: A substreams branch on a kill/shutdown/deprecate event that emits `BalanceChange { balance: vec![0] }` or `add_balance_change` with zero for every token, or a lifecycle flag written into static_att; A custom `active` attribute, or `paused` written with value 0, or a creation handler that ignores the pool's initial enabled flag; change_component_pause_state(.., true) with no path that emits `false`; pause logic driven by a value the module already watches (fee word, flag slot) but only on the transition to the bad value; A new package whose ABI contains Paused/Unpaused events or a whenNotPaused modifier with no handler and no README note
Applies: substreams · look first at lines matching `pause|paused|kill|shutdown|deprecat|disable|enable|active|balance|static|creat|whenNotPaused`
Checked by: repo-script `substreams-pause-without-unpause`; ast-grep `substreams-untradable-without-pause-attribute`; ast-grep `substreams-handwritten-paused-attribute`
