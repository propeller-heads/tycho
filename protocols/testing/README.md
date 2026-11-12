# Protocol Testing

Rust-based integration testing framework for Tycho protocol implementations. See our full
docs [here](https://docs.propellerheads.xyz/tycho/for-dexs/protocol-integration/3.-testing).

## How to Run Locally

```bash
# Ensure PostgreSQL is running or start it via Docker
docker compose up db -d

# Export necessary env vars
export RPC_URL=..
export SUBSTREAMS_API_TOKEN=..

# If you use a local PostgreSQL instance, set the connection string if necessary
# By default, the binary will use `postgres://postgres:mypassword@localhost:5431/tycho_indexer_0`
# export DATABASE_URL=postgresql://postgres:password@localhost:5432/postgres

# Run the tests for a specific package, defined in their integration_test.tycho.yaml file
# This type of tests are constrained to a specific block range defined
cargo run -- range --package "ethereum-balancer-v2"

# To run the full test, that will index from the protocol creation block to the latest:
cargo run -- full --package "ethereum-balancer-v2"

# Run tests on a specific chain. Default is Ethereum.
# Make sure to set the RPC_URL environment variable to match the target network.
cargo run -- range --package "base-aerodrome-slipstreams" --chain base

# Clean up
docker compose down
```

### Running alongside another Tycho stack

The defaults collide with a locally running Tycho stack (Postgres on host port 5431, indexer
on port 4242). Worse, each run drops and recreates the database named in `DATABASE_URL` — so
never point it at a database another instance is using. To run in parallel, isolate all ports:

```bash
# Start a dedicated Postgres on a free host port
DB_HOST_PORT=5433 docker compose up db -d

# Point the test runner at it and pick a free indexer port
export DATABASE_URL=postgres://postgres:mypassword@localhost:5433/tycho_indexer_0
export TYCHO_SERVER_PORT=4243   # or pass --tycho-server-port 4243

cargo run -- range --package "ethereum-balancer-v2"
```

## Strict mode

A package's `integration_test.tycho.yaml` (and each clone's `integration_test_<clone>.tycho.yaml`)
chooses which checks its tests run:

```yaml
harness: strict
substreams_yaml_path: ./substreams.yaml
# ...
```

- **Legacy** (no `harness` key, the default) runs the checks every existing package was written
  against. Its checks and their pass or fail are unchanged.
- **Strict** is opt-in. It is being built up check by check, and is not required of any package
  yet; the yaml format stays backwards compatible until it is complete.

In strict mode:

- A key the runner does not read is an error that names it, so a misspelt `skip_exectuion` cannot
  pass silently. Legacy mode logs such keys as a warning.
- Every skip is written as its reason, a string. `true` gives no reason and `false` is the
  default, so both are errors naming the flag and the component:

  ```yaml
  skip_execution: "range predates Shanghai; the router needs PUSH0"
  ```

  A reason works in legacy mode too, where `true` and `false` stay valid. A trace, a command and
  its output, or a ticket go in the reason. The runner logs it once per component, and the report
  records it.
- A `skip_execution` next to a `skip_simulation` is an error: execution never runs without
  simulation, so its reason would be read nowhere.
- A skip that reads as yes or no, such as `no` or `"false"`, is an error in both modes, since
  `no` would otherwise turn the check off.
- Every check of a test runs, and the test fails at the end listing every failed check, instead of
  stopping at the first. A check that needs the result of a failed one is reported as not run.
- Every expected component's balances are checked, against where the component declares they are
  held, and `skip_balance_check` is an error (below).
- Every direction of every expected component declares what kind of limit `get_limits` reports,
  and the limit is tested as declared (below).
- An execution must pay exactly the simulated amount, unless the package declares a tolerance
  (below, "Execution tolerance").
- New checks are added to strict mode only, so a legacy package never starts failing because the
  harness grew.

A missing executor deployment is not a reason to skip execution, in either mode, when the
protocol's executor has a runtime bytecode fixture in `src/execution.rs`: the harness overrides the
executor address with that bytecode, so execution runs either way. A protocol without one needs a
fixture added first.

### Balances

Legacy mode checks every component in the snapshot against `balanceOf(component id)`, or skips the
check for the whole package. Strict mode checks each expected component, and reports each
separately, against the amounts the package's balance lens reads on chain:

```yaml
harness: strict
balance_lens: ComponentIdBalances
```

A lens is a Solidity contract implementing `IBalanceLens`
(`protocols/adapter-integration/evm/src/balance-lenses/IBalanceLens.sol`). It takes a component's
id, its tokens and its attributes, and returns, for each token, the amount the indexed balance
must equal. The runner builds it with forge, as it builds an adapter, and calls it once per
expected component with an `eth_call` at the snapshot block, placing the lens's code by a state
override, so nothing is deployed and any RPC that serves overrides works.

Two stock lenses cover the common cases:

| Lens | Reads |
|---|---|
| `ComponentIdBalances` | each token's `balanceOf(component id)`, for a component that holds its own tokens |
| `BalanceOwnerBalances` | each token's `balanceOf` of the address in the `balance_owner` attribute, for a component whose tokens sit at an address that holds only its own |

A package whose amounts sit elsewhere writes its own lens, next to its adapter under
`protocols/adapter-integration/evm/src/`, inheriting `BalanceLens` for its helpers (the native
balance, an attribute by name, an attribute as an address or a uint). A singleton vault's per-pool
view, protocol accounting kept as a number, or a token held as another token's balance are read
there, with the protocol's constants in the contract, and components read differently are
branches of one lens. A token the protocol does not account per component reads 0, and the
indexer must then report no balance for it:

```solidity
/// Lido: the pooled ether is protocol accounting; stETH and wstETH are traded, but no balance
/// is tracked for them.
contract LidoBalances is BalanceLens {
    function balances(bytes calldata, address[] calldata tokens, Attribute[] calldata)
        external
        view
        returns (uint256[] memory amounts)
    {
        amounts = new uint256[](tokens.length);
        for (uint256 i = 0; i < tokens.length; i++) {
            if (tokens[i] == address(0)) {
                amounts[i] = ILido(STETH).getTotalPooledEther();
            }
        }
    }
}
```

- The lens gets the id's bytes when it is hex, else its UTF-8 bytes, and the static attributes
  followed by the state attributes at the snapshot block, so the last entry of a name is current.
- The comparison is exact, unless the component is `tvl_only` (below), and a token without an
  indexed balance counts as zero.
- A lens that reverts, or returns a different number of amounts than the component has tokens,
  fails the component, naming the lens.
- A component the lens cannot read, such as a singleton that keeps liquidity rather than
  per-pool amounts, sets `skip_balance_check` with the reason; its `balances` is reported
  `skipped`, and every other check of it still runs. A lens is required unless every expected
  component skips the check. Write a lens rather than a skip wherever the chain has the amount: a
  skip never makes a failing balance check pass.
- A test in strict mode must list its expected components: an empty list, which elsewhere means
  every component of the protocol, would leave the balances unchecked.
- The package-level `skip_balance_check` is an error in strict mode, whatever its value.

`balance_lens` and a component's `skip_balance_check` are read in legacy mode, where they are
ignored.

#### Balances that feed only TVL

Some packages index a balance that drifts slightly from the chain for a known reason, such as a
fee bucket or pending deposits the contract also holds. When simulation does not read those
balances, being slightly off only moves TVL, and the component can declare a tolerance:

```yaml
  - id: "0x..."
    tvl_only:
      bps: 10        # of the on-chain amount; or `wei`, or both
      reason: "the indexed reserves exclude the protocol fee bucket the contract also holds; 6.2 bps at most over 40 components, blocks 21000000-21000100 (range --report)"
```

- The tolerance takes the same fields as `execution_tolerance` (below), checked the same way in
  both modes, with `wei` in base units of each token. A component listed in several tests
  declares the same `tvl_only` in each. It applies to every token, those the lens reads as 0
  included: their indexed balance may then be up to the tolerance's `wei`.
- **The runner verifies the declaration** (`balances.tvl_only`). It decodes the snapshot again
  with the component's balances scaled down, each token's by a different factor (1/2, 1/3, ...,
  in token address order) so their ratios change too, and every limit, spot price and quote at
  0.1%, 1% and 10% of the limit must come out the same, in every direction. If one changes, simulation reads the
  balances, and the check fails, naming each change:
  ```
  WBTC -> WETH: limits is max input 8384236, max output 683094418215225792, but max input 3889669, max output 499999999548736461 with the balances scaled down
  ```
- A component with no nonzero indexed balance fails the check, as scaling verifies nothing. So
  does one whose every limit, spot price and quote errors: nothing was compared. An error counts
  as the same only when both states give the same message.
- Strict mode rejects `tvl_only` on a component that sets `skip_simulation`, since the check
  quotes it.

### Limits

The ladder trades at most 10% of a direction's limit, so on its own it passes a limit ten times too
high. Strict mode also tests the limit itself, per direction, after the `int128` cap the runner
applies for execution. Every direction of every expected component declares the kind of limit
`get_limits` reports, unless the component skips simulation. A `limits` value is one declaration for every direction, or a map from
directions to declarations, keyed `0x<token in>->0x<token out>`, with an optional `default` for the
directions it does not list. A package declares limits for all its components, and a component can
override them:

```yaml
harness: strict
limits: {kind: soft, reason: "constant product: any input trades, at a worse price"}
tests:
  - name: ...
    expected_components:
      - id: "0x..."                    # a pool: soft, from the package
      - id: "0x..."                    # a vault: a deposit cap one way, redemptions the other
        limits:
          "0x<asset>->0x<share>": {kind: hard}
          "0x<share>->0x<asset>":
            kind: under_cap
            reason: "the redemption limit depends on a keeper-set rate we do not index (cast call 0x... 'maxRedeem(address)' -b 21000000 returns ...); gives up to 2% of the vault's redeemable shares"
```

For each direction the most specific declaration applies: the component's entry for that direction,
the component's default, the package's entry for that direction, then the package's default. A
direction must be one of the component's token pairs (for a package entry, of some expected
component's).

| Kind | Meaning | Required | Tested |
|---|---|---|---|
| `hard` | the venue rejects any input above its bound, and the limit is that bound | optional `rounding` | at the limit; past the bound, the quote errors and the swap reverts |
| `soft` | the venue has no bound on the input; the limit is a recommended size, as the `ProtocolSim::get_limits` documentation allows | `reason`, naming the code that shows it | at the limit; twice the limit still quotes |
| `under_cap` | the venue has a bound, and the limit stays below it because the exact bound cannot be derived | `reason`: why, such as a command and its output, and the capacity given up | at the limit; one unit past it, the quote and the swap succeed |

- **At the limit** (`limits.at_limit`): `get_amount_out(max_input)` succeeds, and executing it
  does not revert and matches the simulation.
- **Max output** (`limits.max_output`): the quote at `max_input` pays no more than `max_output`.
- **Past the limit** (`limits.past_limit`): the venue behaves as the kind claims, so a wrong
  declaration fails.
  - `hard`: `get_amount_out(max_input + rounding + 1)` errors, and executing it reverts.
    `rounding` is 0 unless declared: where `get_limits` derives the input from an output-side
    bound with a rounding division, the bound can exceed the limit by a few units, and
    `{kind: hard, rounding: 2}` probes past them. The swap's minimum output is set from the quote
    at the limit, so a venue that accepts the input executes the swap and fails the check. The
    check accepts any revert from the chain, but not a node error on the call, which proves
    nothing.
  - `under_cap`: `get_amount_out(max_input + 1)` succeeds, and executing it does not revert, so
    the bound is above the limit.
  - `soft`: `get_amount_out(2 × max_input)` succeeds, so the venue has no bound near the limit.
    Nothing past it is executed.

  When the limit is at or above the `int128` cap, the cap is not the venue's bound, and nothing is
  probed past it.

Each of the three checks is reported per component and direction, with `component` and
`direction` keys in the report. A check passes only once everything it tests ran. Unless the
simulation already failed it:

- `limits.past_limit` is skipped for a limit at the cap.
- When the quote at the limit fails, `limits.max_output` is `not_run`, and so is
  `limits.past_limit` for a hard or under-cap limit. When the swap at the limit does not execute,
  `limits.past_limit` is `not_run` too: what happens past a limit that does not execute either
  proves nothing.
- A component with `skip_execution` runs the checks in simulation only: `limits.at_limit` and
  `limits.past_limit` are skipped with its reason.
- A component with `skip_simulation` has all three skipped.
- A direction whose limit is zero at the tested block has all three skipped. A declared direction
  the runner never quotes, because the indexed tokens differ from the expected ones, fails all
  three.

A declaration missing a required field, or with a field its kind does not read, or naming a
direction its component does not have, or naming one direction twice, is an error in both modes.
Legacy mode reads the declarations but runs none of these checks.

### Execution tolerance

An execution matches its simulation when the executed amount out is within the tolerance: a number
of basis points of the executed amount, a number of base units of the token out, or either. Legacy
mode allows 0.5% for every package. Strict mode, in `range` and in `full`, requires an exact match:
a simulation that ports the venue's math, or runs its bytecode, should not differ by
a wei, and 0.5% hides rounding and fee bugs. Where exact is out of reach, the package declares a
tolerance, with a reason that says why and what a run measured:

```yaml
harness: strict
execution_tolerance:
  bps: 0.02
  reason: "the vault prices from a share price rounded to 6 significant digits; 0.011 bps at most, blocks 21000000-21000100 (range --report)"
```

`bps` is relative to the executed amount, with up to two decimals, from 0.01 to 9999.99. Where
the loss is a fixed amount whatever the trade's size, such as stETH's 1-2 wei per transfer,
declare `wei` instead, in base units of the token out; a bps loose enough for a small trade allows
ether on a large one. `wei` is one amount for every token, or an amount per token address with an
optional `default`, since one raw amount means different things for tokens of different decimals:

```yaml
execution_tolerance:
  wei: {default: 2, "0xA0b86991c6218b36c1d19d4a2e9eB0cE3606eB48": 1}
  reason: "stETH transfers move shares, rounding the amount down by up to 2 wei; 1 wei at most, blocks 25603204-25603404 (range --report)"
```

For a known artifact, declare its documented bound, and give the run in the reason as support: a
short range can show less than the bound, as this one shows 1 wei, and a tolerance cut to the run
fails at random.

A component can declare its own `execution_tolerance`, which overrides the package's for its
executions, so a pool with a known artifact does not loosen every other pool of the package. A
component listed in several tests declares the same one in each.

With both `bps` and `wei` declared, a difference within either passes. A mismatch says whether the
simulation over- or underquotes, since those are different defects. At least one of `bps` and
`wei` is required, and a reason, in both modes; legacy mode keeps 0.5% whatever is declared. The
report's `drift` gives the largest difference of each test, within tolerance or not, so the
number a reason gives can be checked against a run.

### Streamed range

In strict mode, `range` streams the range while it is indexed; `--no-stream` turns it off. The
runner starts the indexer with `--wait-for-subscriber`, so extraction starts only once the runner
has subscribed, and every block reaches it: the indexer waits for room in each subscription's
channel, a resubscription's included. Every block's deltas then go through tycho-client and the
production decoder. The client follows only the expected components, with a large buffer, and
drops a component that does not decode, so a `skip_simulation` component that cannot decode does
not fail the stream. The runner checks a sample of the blocks, not every one:

| Every... | What runs | Cost |
|---|---|---|
| block | its deltas are applied to the decoded state | nothing extra |
| block in which an expected component changed, up to 50 spread evenly over the range | `sim.ladder` and the limit checks | local, no RPC |
| of those, the fewest spread evenly that meet the floor below | `exec.ladder` and the limit executions | one node call per execution |

Executions run after the stream ends, at each chosen block, so the stream never waits on the node.
Checks at a streamed block carry its `block` in the report. At a block that only simulates, the
limit checks whose swap was queued are `skipped`, since nothing executed there. When every
component skips execution, nothing is executed and `exec.ladder` is skipped.

A stream that ends before the stop block, or skips a block, fails `stream.matches_snapshot`, naming
the last block it reached or the missing blocks.

The indexer keeps no version history (`run` takes the current time as its retention horizon), so
the client's snapshot of a component created inside the range is read at the latest state, not at
the creation block. A component that changes again before the client fetches its snapshot can
therefore stream from a later state; `stream.matches_snapshot` then fails, so the race shows as a
failure rather than a wrong pass.

After the stream:

- **`stream.matches_snapshot`**: the snapshot at the stop block, decoded as range mode always does,
  must quote exactly as the streamed state: every direction's limits, spot price and ladder quotes.
  A delta applied twice, a dropped field or a stale attribute fails here even when no single
  block's quote looks wrong.
- **`stream.coverage`**: how much the range exercised, against this floor. It is report-only for
  now: a shortfall is a `warn` naming what is missing, and does not fail the test.

  | Measure | Floor |
  |---|---|
  | blocks in which an expected component changed | 20 |
  | changed blocks per simulated component | 2 |
  | execution checks, in total | 100 |
  | execution checks per component with execution on | 10 |
  | tradable directions of the simulated components quoted after a delta | all |

  The report's `coverage` section has the counts per test.

The stream follows only the expected components, including one created inside the range.

### Full mode

`full` indexes from the protocol's start block and then tests every update the stream delivers,
for every pool of the protocol, not only the expected components. A legacy package runs the ladder
and executes its quotes within 0.5%, logs whatever fails, and keeps going.

A strict package runs, on every update:

- the ladder (`sim.ladder`) and its executions (`exec.ladder`), within the package's tolerance;
- the limit checks (`limits.at_limit`, `limits.past_limit`, `limits.max_output`) on every
  direction with a declaration: an expected component's own `limits`, or else the package's.
  A pool the tests do not list gets the package's `limits`; without them, its limits are not
  checked.

Skips do not apply: a skip covers one test's range, and `full` has none. The first update with a
failed check ends the run with an error naming the block and the checks; `full --report <path>`
writes that block's checks as JSON, in the report format below. A block the node does not serve,
or executions it cannot simulate, are logged and the update is passed over.

### Test report

`range --report <path>` writes every check's outcome, per test, as JSON, in either mode:

```json
{
  "package": "ethereum-foo",
  "chain": "ethereum",
  "harness": "strict",
  "results": [
    { "test": "first", "check": "state.components", "outcome": "pass" },
    { "test": "first", "check": "balances", "outcome": "pass" },
    { "test": "first", "check": "exec.ladder", "component": "0xabc...", "outcome": "skipped", "reason": "range predates Shanghai; the router needs PUSH0" },
    { "test": "first", "check": "sim.ladder", "outcome": "fail", "detail": "Error calculating amount out ..." },
    { "test": "first", "check": "exec.ladder", "outcome": "not_run", "reason": "sim.ladder failed" },
    { "test": "second", "check": "limits.past_limit", "component": "0x...", "direction": "0x...->0x...", "outcome": "pass",
      "limit": { "kind": "soft", "reason": "constant product: ...", "declared_by": "package_default", "max_input": "2162000", "max_output": "68309", "capped": false } },
    { "test": "second", "check": "sim.ladder", "block": 19701407, "outcome": "pass" },
    { "test": "second", "check": "stream.coverage", "outcome": "warn", "detail": "4 blocks changed an expected component, under the floor of 20; ..." }
  ],
  "drift": [
    { "test": "second", "simulation": "test_0x..._USDC_WETH_1000000", "component": "0x...", "simulated": "1003", "executed": "1000", "bps": -30.0, "largest_difference": "-3", "largest_difference_token": "WETH" }
  ],
  "coverage": [
    {
      "test": "test_wolf_weth_pool",
      "blocks_streamed": 105,
      "changed_blocks": 4,
      "changed_blocks_by_component": { "0x6732...18c2": 4 },
      "simulated_blocks": [19701398, 19701407, 19701458, 19701496],
      "executed_blocks": [19701398, 19701407, 19701458, 19701496],
      "executions": 32,
      "executions_by_component": { "0x6732...18c2": 32 },
      "quoted_directions": { "0x6732...18c2": ["0x6746...7e69->0xc02a...6cc2", "0xc02a...6cc2->0x6746...7e69"] },
      "tradable_directions": { "0x6732...18c2": ["0x6746...7e69->0xc02a...6cc2", "0xc02a...6cc2->0x6746...7e69"] }
    }
  ]
}
```

| Check | What it checks |
|---|---|
| `index` | the range indexes, and the snapshot can be read at the stop block |
| `state.components` | every expected component is indexed, with its tokens and static attributes |
| `state.excluded` | no excluded component is indexed |
| `balances` | indexed balances match `balanceOf` on chain; strict: per component, against the amounts the package's `balance_lens` reads, or `skipped` with a component's `skip_balance_check` |
| `balances.tvl_only` | strict: a `tvl_only` component quotes the same with its balances scaled down |
| `decode` | the snapshot decodes, and so does every expected component |
| `sim.ladder` | limits, spot prices and quotes at 0.1%, 1% and 10% of the limit, every direction |
| `exec.ladder` | executing each quote on chain matches the simulation, within the tolerance |
| `limits.at_limit` | strict: every direction quotes and executes at its limit |
| `limits.past_limit` | strict: past the limit, the venue behaves as the declared kind claims |
| `limits.max_output` | strict: the quote at the limit pays no more than the limit's max output |
| `stream.matches_snapshot` | strict, streamed: the streamed state quotes exactly as the snapshot at the stop block |
| `stream.coverage` | strict, streamed: the range meets the floor; a shortfall is a `warn`, for now |

Every `limits.*` result carries its `limit`, whatever the outcome: the declared `kind`, its
`reason` (and a hard limit's `rounding`), which declaration applies (`declared_by`: `component`,
`component_default`, `package` or `package_default`), and `get_limits`' `max_input` (after the
`int128` cap) and `max_output`, with `capped` saying whether the cap applied. The run also logs a
count per test, such as `limits: 3 hard, 8 soft, 1 under_cap`.

A skipped check is reported with its reason, per component (by lowercase id) for
`skip_simulation` and `skip_execution`; a legacy `true` reads "no reason given". A test-wide
`sim.ladder` or `exec.ladder` result covers only the components without a `skipped` result for
it. When every component skips, `range --report` records the test-wide result as `skipped`,
unless an earlier check failed, in which case it is `not_run`. In legacy mode the report ends at
the first failed check of a test. `drift` has one entry per test with at least one execution that
paid a nonzero amount: of those, the largest `(executed - simulated) / executed` in magnitude, in
bps, kept with its sign (negative when the simulation overquotes), and the largest
`executed - simulated` in magnitude, in base units of the token out named by
`largest_difference_token`, which may come from another execution.

## How to Run with Docker

```bash
# Export necessary env vars
export RPC_URL=..
export SUBSTREAMS_API_TOKEN=..
export PROTOCOLS="ethereum-balancer-v2=weighted_legacy_creation ethereum-ekubo-v2"

# The test-runner image compiles the EVM adapter against its lib/ submodules, so initialize them
# first
git submodule update --init --recursive ../adapter-integration/evm/lib

# Build both images (test-runner + db) and run the tests. --abort-on-container-exit stops the
# stack when the one-shot test-runner finishes.
docker compose up --build --abort-on-container-exit

# Clean up
docker compose down
```

By default this runs `range` tests. To run the `full` test (continuous sync from the initial block
to the chain tip) set `MODE=full`. In full mode the optional `=` suffix is the start block
(`--initial-block`). Full mode runs until a strict package fails a check, and a legacy one never
exits, so omit `--abort-on-container-exit` and tear down manually:

```bash
export MODE=full
export PROTOCOLS="ethereum-balancer-v2=12345678"
docker compose up --build
```

## Runtime Bytecode Fixtures

Execution validation overrides the TychoRouterV3, FeeCalculator, and protocol executors at simulation
time with the runtime bytecode in `fixtures/*.runtime.json`. These are generated from the
`tycho-execution` contracts, so they must be regenerated whenever those contracts change.

`crates/tycho-execution/contracts/test/RuntimeBytecodeFixtures.sol` lists every fixture and reads
executor constructor arguments from `crates/tycho-execution/config/executor_deployments.json`.
Executor fixtures deploy at `tycho-test`'s `EXECUTOR_ADDRESS`, where execution validation plants
their code, so immutable self-addresses remain callable. The router and fee calculator use
addresses derived from their contract names. Each fixture has its own fork; an executor forks
its deployment chain at a pinned block, unless its constructor needs state from a different block
on that chain.

```bash
cd ../../crates/tycho-execution/contracts
export RPC_URL=..   # and the other [rpc_endpoints] in foundry.toml, one per chain with a listed executor

# Verify the committed fixtures match the current contracts (forge test runs this in CI)
forge test --match-contract RuntimeBytecodeFixtures

# Regenerate every fixture from the current contracts
forge script script/WriteRuntimeBytecodeFixtures.s.sol
```

The FeeCalculator fixture is a fresh deployment with zero fees, so it is a no-op during simulation
(the router calls it on every swap to read the router fee rate).

`UniswapV4Robinhood.runtime.json` builds like the others, from `(robinhood, uniswap_v4)` on a
Robinhood fork, so `forge test` catches drift if the Robinhood executor changes.
