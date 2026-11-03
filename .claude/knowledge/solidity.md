# Coding in Solidity (Foundry)

## Commands

During development, from `crates/tycho-execution/contracts/`:

```bash
forge build                      # compile
forge test -vvv                  # run all tests (requires RPC_URL + BASE_RPC_URL)
forge test -vvv --match-test foo # run a single test
forge fmt                        # auto-format
forge fmt --check                # format check (CI)
forge snapshot                   # update gas snapshots
```

Static analysis (requires Python env — see
[contributing guidelines](https://docs.propellerheads.xyz/tycho/for-dexs/protocol-integration/contributing-guidelines#contract-analysis)
for setup):

```bash
slither .                        # run from contracts/
```

After a task is done, run `forge fmt` and the tests covering what you changed (e.g.
`forge test --match-contract TychoRouterTest`). Save the `/run-ci` skill for when you open a PR or
change something workspace-wide.

Executor conventions (hardcoded transfer data, no amount handling in `swap()`) and the security
checklists are in `crates/tycho-execution/CLAUDE.md`.

## Testing

- Fork tests require `RPC_URL` (Ethereum mainnet) and `BASE_RPC_URL` env vars

How Rust-encoded calldata reaches the Solidity tests (`calldata.txt`) is described in
`crates/tycho-execution/CLAUDE.md` § Conventions › Testing.

## Project Config

- Solidity dependencies managed as git submodules under `contracts/lib/` — always clone with
  `--recursive`

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds). `forge fmt` and `forge
lint` check formatting and part of the naming.

<!-- pieces: rendered from candidates.json by candidates/knowledge.py; edit the pieces there, not below -->

### immutable-code-comments-state-mechanism
Comments in code that will be deployed immutable state the mechanism, not operator-set figures measured at one point in time; dated figures go in docs that can be updated.
Why: Comments in code deployed immutable (e.g. a verified smart contract) cannot follow the operator, so the figures become wrong and stay wrong.
Spot: Comments in code that cannot change after deployment (smart contracts, flashed firmware, published artifacts) giving rates, thresholds or fee values that an external operator sets.
Applies: solidity · look first at lines matching `//|/\*|\*|@notice|@dev`

### per-chain-differences-via-deployment
Per-chain differences in a contract come from deployment, not source: addresses that differ per chain are constructor-set immutables, and features present on only some chains live in chain-specific subclasses of an abstract base overriding a virtual hook, never in `constant` literals or sentinel/dead-address constructor args.
Why: A hard-coded constant forces a source edit (and different audited bytecode) per chain or silently points at the wrong contract; sentinels deploy unreachable code and make 'unset' indistinguishable from misconfiguration.
Spot: `address private constant FOO = 0x...` for a contract with per-chain deployments; constructor args documented as 'pass a dead address when unsupported'; `if (addr != SENTINEL)` branches.
Applies: solidity · look first at lines matching `address|constant|immutable|constructor|0x|chain|sentinel|dead`
Checked by: ast-grep `sol-address-literal-constant`; ast-grep `sol-constructor-zero-address-sentinel`; repo-script `deploy-args-sentinel`

### forge-script-writes-generated-files
Generated files are written by a `forge script`; `forge test` only checks them, never writes them behind an env flag.
Why: forge runs tests in parallel, so a test that lists or checks files races the writer and fails on the documented regenerate command; an exported write flag also lets CI regenerate and pass.
Spot: `vm.envOr("..._WRITE", false)` gating `vm.writeFile` inside a `test*` function; failure messages telling the user to rerun `forge test` with a flag.
Applies: solidity, solidity-test · look first at lines matching `writefile|envor|write|vm\.env|ffi`
Checked by: ast-grep `sol-test-writes-files`

### custom-errors
Reverts use custom errors, not `require(cond, "string")` or short string codes.
Why: String reverts cost more gas and bytecode, are inconsistent with the codebase's error convention, and short codes like "U" are undecodable for callers.
Spot: New `require(..., "...")` or `revert("...")` in contract code.
Applies: solidity · look first at lines matching `require|revert|error`
Checked by: forge-lint `forge-lint custom-errors` (full)

### packed-calldata-length-check
A minimum-length check on packed calldata covers every fixed-size field the decoder then reads, including an embedded 4-byte selector and any offsets it will later patch.
Why: An undercounted bound lets a short payload through the check and fails later with an opaque out-of-bounds read or, with assembly, reads garbage.
Spot: `if (data.length < N)` where N is the header sum but the code subsequently slices a selector or fixed-offset fields from the trailing payload.
Applies: solidity · look first at lines matching `length|calldata|bytes|\[\d|selector|offset|slice|:\]`
Checked by: repo-script `sol-packed-length-check`

### forward-pre-encoded-args
When a contract only forwards arguments to another contract's entrypoint, it accepts them pre-encoded (packed, or full calldata with selector) and forwards them, instead of `abi.decode`-ing them into locals just to re-encode the call.
Why: Decoding and re-encoding costs gas and code for no added safety: the callee's ABI decoder already validates the arguments and the outer length is already checked.
Spot: `abi.decode(args, (...))` immediately followed by a single external call that passes exactly those decoded values through. Pair with a selector allowlist if the forwarded blob includes the selector.
Applies: solidity · look first at lines matching `abi\.decode|abi\.encode|call|args|forward`

### deterministic-address-test-and-script
Deterministic-address helpers meant to give the same address under both `forge test` and `forge script` do not rely on CREATE/CREATE2/CREATE3 from the calling contract; they place code at an address derived from the input alone (e.g. `deployCodeTo` at `address(bytes20(keccak256(name)))`).
Why: CREATE3 addresses depend on the deployer, which differs between test and script contracts, and contracts that cache `address(this)` (e.g. OpenZeppelin EIP712's `_cachedThis`/domain separator) bake that address into their runtime bytecode.
Spot: CREATE3/CREATE2 deploys from a shared base used by both a `.t.sol` and a `.s.sol`, where bytecode or addresses are compared across the two.
Applies: solidity, solidity-test · look first at lines matching `create|deploy|salt|address|keccak|deploycodeto`

### transient-slot-names
Transient storage slot constants are keccak256 of a descriptive name string.
Why: Hand-picked or sequential slots risk collisions and say nothing about what they hold.
Spot: Transient slot constants defined as literals or small integers.
Applies: solidity · look first at lines matching `transient|tstore|tload|slot`
Checked by: repo-script `sol-slot-constant-keccak` (full)

### assembly-and-slither-suppressions
Inline assembly is avoided unless no high-level alternative is viable, and every Slither suppression is `// slither-disable-next-line` with a comment giving the reason.
Why: Assembly bypasses compiler checks; unexplained suppressions hide real findings.
Spot: New assembly blocks; slither-disable lines with no justification.
Applies: solidity, solidity-test · look first at lines matching `assembly|slither`
Checked by: repo-script `sol-slither-suppression-reason`; forge-lint `forge-lint inline-assembly (Foundry >= 1.8.5)`

### explicit-admin-transitions
An administrative state transition (activating a timelocked setting, rotating a configured address) happens in an explicit call by the authorised role, not lazily inside a user-facing function.
Why: Lazy activation makes some random user's transaction pay the extra storage writes and emit an unrelated event, and the change takes effect at an unpredictable moment.
Spot: A user entrypoint, or a helper it calls, that checks a pending value's expiry and writes the active slot or emits an admin event (e.g. `_activeFeeCalculator()` called from swap).
Applies: solidity
Checked by: ast-grep `sol-lazy-admin-activation`

### paginated-collection-views
A view that returns an on-chain collection with no fixed size takes a range (start, count), so callers can page through it.
Why: Returning the whole set eventually exceeds the gas or memory limit that RPC nodes apply to eth_call, and the getter stops working.
Spot: A view that loops over `EnumerableSet.length()`, or over an array that admins or users can grow, and returns everything it visits.
Applies: solidity · look first at lines matching `view|length|for\s*\(|enumerable|\[\]`
Checked by: ast-grep `sol-unpaginated-collection-view`

### bound-replaceable-contract-output
A contract that delegates a computation to a replaceable external contract (a configurable calculator or oracle) computes the resulting payout itself, and bounds what the external contract returns before using it (total fees never exceed the output).
Why: A buggy or malicious replacement could otherwise drain or misdirect funds through a value the caller trusted blindly.
Spot: A call to an admin-settable contract whose return value is used directly as the amount sent to the user, or whose returned fee amounts are transferred without comparing their sum to the available balance.
Applies: solidity

### permissionless-balance-policy
A contract whose permissionless entry point can move any token balance the contract holds either restricts who may call it or documents that any held balance is claimable by anyone and treated as lost, and it does not keep an admin rescue function that the open path can front-run.
Why: Leftover balances (rounding dust, mistaken transfers) are taken first-come-first-served, and the admin-only rescue protects nothing.
Spot: An external swap/forward function without a caller check whose token, amount and recipient are caller-supplied, next to an onlyRole rescue/sweep function.
Applies: solidity
Checked by: ast-grep `sol-rescue-beside-open-entrypoint`

### reserve-gas-for-catch-path
A try/catch whose catch path must still do gas-consuming work reserves gas for it out of what the caller actually supplied, since the EVM forwards at most 63/64 of the remaining gas and silently clamps an explicit `{gas: cap}` that exceeds it.
Why: A callee that fails by exhausting gas leaves the catch path about 1/64 of the budget, so the fallback reverts too; a fixed cap only helps when the transaction happens to carry far more gas than the cap.
Spot: `try this.f()` or `try ext.f{gas: X}()` followed by a catch arm that runs another swap or external call.
Applies: solidity · look first at lines matching `try|catch|gas`
Checked by: ast-grep `sol-catch-path-runs-calls`

### solidity-enum-dispatch-exhaustive
An if/else chain dispatching on an enum names every variant explicitly and ends in an `else` that reverts with an unknown-value error; the decode-time bounds check uses `type(E).max`, not the currently last variant.
Why: A bare final `else` routes a newly added variant to the last handler with the caller's bytes, and a bounds check naming the last variant goes stale.
Spot: A dispatch chain whose last branch is `else { _handleLastVariant(...) }`; bounds checks like `> uint8(Enum.LastVariant)`.
Applies: solidity · look first at lines matching `enum|else|if\s*\(|uint8\(|type\(|max`
Checked by: ast-grep `sol-enum-dispatch-bare-else`

### self-call-only-functions-check-sender
Every external function that exists only to be self-called under try/catch checks `msg.sender == address(this)` itself, including entry points that are reached only through another self-only function.
Why: Without the check on each entry point, anyone can call the inner function directly and run a swap or simulation with arbitrary arguments.
Spot: `function x(...) external` documented as 'external only so ... can try/catch it' without a self-caller guard.
Applies: solidity · look first at lines matching `external|try|this\.|msg\.sender|self`
Checked by: ast-grep `sol-self-call-without-sender-check`

### no-magic-amount-modes
A function that returns an amount never encodes a mode in a magic amount value; the mode is a separate boolean hook or return value.
Why: Callers must remember which amount is magic, and a real amount can collide with the sentinel.
Spot: A constant like PRIMARY_FIRST returned from a `_quote*` function and compared with `!=` by the caller.
Applies: solidity · look first at lines matching `_quote|quote|amount|constant|return|!=|==`
Checked by: ast-grep `sol-magic-amount-mode`

### import-narrowest-declaring-type
A contract imports and casts to the narrowest contract or interface that declares the members it uses, not a top-level contract that pulls its whole dependency graph into the compile.
Why: Importing the top-level contract recompiles it with every user, couples the user to members it never calls and hides which part of the interface it depends on.
Spot: An import of a top-level contract (`TychoRouterV3`, a router or registry) in an executor, helper or test that calls one or two members declared on a base contract or interface (`Dispatcher`); a cast `TopLevel(payable(addr)).member()`.
Applies: solidity, solidity-test · look first at lines matching `import|payable\(`

## Libraries

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| solady-libstring | check whether a string ends with a suffix (or similar string operation) in Solidity | solady `LibString.endsWith` (`@solady/utils/LibString.sol`; crates/tycho-execution/contracts/lib/solady/src/utils/LibString.sol:585) | a hand-written loop comparing `bytes(text)[len - s.length + i]` against `bytes(suffix)[i]` | A private `_endsWith`/`_startsWith`/byte-comparison loop over `bytes(string)` in a contract or test that already imports solady. | solidity, solidity-test |
| foundry-typed-json-cheatcodes | read a typed array/value from a JSON config in a Foundry test or script | forge-std `Vm.parseJsonUintArray` and siblings (crates/tycho-execution/contracts/lib/forge-std/src/Vm.sol:931) | `abi.decode(vm.parseJson(json, key), (uint256[]))` plus a manual re-encoding loop | `abi.decode(vm.parseJson(json, key), (...))` in tests or scripts, especially over user-maintained config files. | solidity, solidity-test |
| foundry-serialize-json | write a small JSON file from Foundry and later check it | forge-std `Vm.serializeBytes` + `vm.writeFile`; `Vm.parseJsonBytes` for the check (crates/tycho-execution/contracts/lib/forge-std/src/Vm.sol:970) | string-concatenated JSON and `keccak256` of file contents | `string.concat('{"key":"', vm.toString(x), '"}')` and `keccak256(bytes(vm.readFile(path)))` comparisons. | solidity, solidity-test |
