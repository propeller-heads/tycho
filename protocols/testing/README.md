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

# To run the full test, that will index from the manifest's initialBlock (or --initial-block) to the latest:
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

### How a range run ends

- `--match-test <text>` runs the test cases whose name contains the text.
- A package without a test yaml, or whose substreams manifest is missing, logs a warning and
  exits successfully: nothing was tested.
- A test whose spkg build or indexing fails fails `index` and ends the whole run: the report is
  written and the later test cases do not run. A test whose snapshot block is off fails `index`
  and the run moves on to the next test.
- Otherwise every test runs, the report is written at the end, and the run fails listing the
  tests that failed.

## Strict mode

A package's `integration_test.tycho.yaml` (and each clone's `integration_test_<clone>.tycho.yaml`)
chooses which checks its tests run:

```yaml
harness: strict
substreams_yaml_path: ./substreams.yaml
# ...
```

- **Legacy** (no `harness` key, the default) runs the checks every existing package was written
  against. Its checks and their pass or fail are unchanged, with one exception that applies in
  both modes: the file name of `substreams_yaml_path` must name the package's chain (below).
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
  stopping at the first. A check that needs the result of a failed one is reported as not run. A
  component whose ladder fails fails `sim.ladder`; the other components are still executed and
  their limits checked, and that component's limit checks are reported as not run.
- Every expected component's balances are checked, against where the component declares they are
  held, and `skip_balance_check` is an error (below).
- Every direction of every expected component declares what kind of limit `get_limits` reports,
  and the limit is tested as declared (below).
- An execution must pay exactly the simulated amount, unless the package declares a tolerance
  (below, "Execution tolerance").
- New checks are added to strict mode only, so a legacy package never starts failing because the
  harness grew. The one exception is the chain in the manifest's name, below.

In both modes, the file name of `substreams_yaml_path` names the chain the package is built for:
its first word, or the word after a `test-` prefix (`base-aero-cl.yaml`,
`test-base-lunarbase.yaml`), and a plain `substreams.yaml` is Ethereum. A name that names no chain
is an error, because `full` and the cluster test pick each chain's packages by it.

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

Three stock lenses cover the common cases:

| Lens | Reads |
|---|---|
| `ComponentIdBalances` | each token's `balanceOf(component id)`, for a component that holds its own tokens |
| `BalanceOwnerBalances` | each token's `balanceOf` of the address in the `balance_owner` attribute, for a component whose tokens sit at an address that holds only its own |
| `UniswapV4Balances` | what a Uniswap V4 pool's liquidity holds at its current price, walked from its ticks through the `PoolManager` in the `balance_owner` attribute. LP and protocol fees and hooks' ERC-6909 claims are not counted, so a package whose indexed balances include them does not match it exactly. The component id must be the 32-byte pool id (otherwise it reverts `NotAPoolId`), and it reads the `tick_spacing` attribute. At tick spacing 1 a call costs tens of millions of gas (`testGasOfAnEmptyPoolAtTickSpacing1` measures it), more than some nodes' `eth_call` gas cap. A package that indexes fees in its balances pairs it with `tvl_only` |

A package whose amounts sit elsewhere writes its own lens, next to its adapter under
`protocols/adapter-integration/evm/src/`, inheriting `BalanceLens` for its helpers (a token's
balance, the native one included, `balancesOf` over all the tokens, an attribute by name, an
attribute as an address). A singleton vault's per-pool view, protocol accounting kept as a number,
or a token held as another token's balance are read there, with the protocol's constants in the
contract, and components read differently are branches of one lens. A token the protocol does not
account per component reads 0, and the indexer must then report no balance for it:

```solidity
interface ILido {
    function getTotalPooledEther() external view returns (uint256);
}

/// Lido: the pooled ether is protocol accounting; stETH and wstETH are traded, but no balance
/// is tracked for them.
contract LidoBalances is BalanceLens {
    address constant STETH = 0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84;

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

Legacy mode does not read `balance_lens` or a component's `skip_balance_check`: it warns about
them as keys the runner does not read, and checks the balances as before.

#### Balances that feed only TVL

Some packages index a balance that drifts slightly from the chain for a known reason, such as a
fee bucket or pending deposits the contract also holds. When simulation does not read those
balances, being slightly off only moves TVL, and the component can declare a tolerance:

```yaml
  - id: "0x..."
    tvl_only:
      bps: 10        # of the on-chain amount; or `wei`, or both
      reason: "the indexed reserves exclude the protocol fee bucket the contract also holds, which reaches 6 bps of the reserves (feeBucket() against getReserves() at block 21000000)"
```

- The tolerance takes the same fields as `execution_tolerance` (below), checked the same way in
  both modes, with `wei` in base units of each token. A component listed in several tests
  declares the same `tvl_only` in each. It applies to every token, those the lens reads as 0
  included: their indexed balance may then be up to the tolerance's `wei`.
- **The runner verifies the declaration** (`balances.tvl_only`). It decodes the snapshot again
  with the component's balances scaled down, each token's by a different factor (1/2, 1/3, ...,
  in token address order) so their ratios change too, and every limit, spot price and quote at
  0.1%, 1% and 10% of the limit must come out the same, in every direction. If one changes,
  simulation reads the balances, and the check fails, naming each change:
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
applies for execution. A strict package declares a package-level default, the kind of limit
`get_limits` reports for every pool of the protocol, since `full` and the cluster test check pools
no test lists against it. A `limits` value is one declaration for every direction, or a map from
directions to declarations, keyed `0x<token in>->0x<token out>`, with an optional `default` for the
directions it does not list (the package's map must have one). A component can override the
package's declarations:

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
| `hard` | the limit is the largest input the venue accepts, whichever side of the swap the venue bounds | nothing | at the limit; one unit past it, the quote errors and the swap reverts |
| `soft` | the venue has no bound on the input; the limit is a recommended size, as the `ProtocolSim::get_limits` documentation allows | `reason`, naming the code that shows it | at the limit; twice the limit still quotes |
| `under_cap` | the venue has a bound, and the limit stays below it because `get_limits` cannot report the largest input the venue accepts | `reason`: why, such as a command and its output, and the capacity given up | at the limit; one unit past it, the quote and the swap succeed |

- **At the limit** (`limits.at_limit`): `get_amount_out(max_input)` succeeds, and executing it
  does not revert and matches the simulation.
- **Max output** (`limits.max_output`): the quote at `max_input` pays no more than `max_output`.
- **Past the limit** (`limits.past_limit`): past the limit, the venue behaves as the kind claims,
  so a wrong declaration fails. Whichever side the venue bounds, `hard` claims that `max_input` is
  the largest input it accepts. A venue that bounds the output, such as a pool's reserve of the
  token out, accepts every input that pays no more than that bound; a `get_limits` that inverts
  the bound with a floor division reports less than that, so one unit past it still quotes, and
  the limit is `under_cap`, with the capacity it gives up in its reason. By kind:
  - `hard`: the quote one unit past the limit, `max_input + 1`, errors, and executing it reverts.
    Its minimum output is 80% of the quote at the limit, which a larger input easily meets: a
    venue that accepts the input executes the swap and fails the check. The check
    accepts any revert from the chain, but not a node error on the call, which proves nothing.
  - `under_cap`: the quote one unit past the limit succeeds, and executing it does not revert, so the
    bound is above the limit.
  - `soft`: `get_amount_out(2 × max_input)` succeeds, so the venue has no bound near the limit.
    Its claim is checked in simulation alone, and nothing past it is executed.

  When the limit is at or above the `int128` cap, the cap is not the venue's bound: nothing is
  probed past it, and `limits.past_limit` is not run. The cap is Uniswap V4's bound on an
  executed amount; both runners apply it to every protocol, including those whose executor takes
  more.

Each of the three checks is reported per component and direction, with `component` and
`direction` keys in the report, and the `limit` it was checked against: the declaration, which
declaration applies, and the sizes when the direction was quoted. A limit check recorded for the
whole test, not run or skipped because an earlier step failed or every component skips, has no
`limit`. A check passes only once everything it tests ran. Unless the
simulation already failed it:

- `limits.past_limit` is `not_run` for a limit at the cap, which is not the venue's bound.
- When the quote at the limit fails, nothing is probed past it, whatever the kind, and
  `limits.max_output` and `limits.past_limit` are `not_run`. For a hard or under-cap limit,
  `limits.past_limit` is also `not_run` when the swap at the limit does not execute, as when the
  execution batch fails: what happens past a limit that does not execute proves nothing. A soft
  limit's `limits.past_limit` is judged in simulation alone, so it passes on its quote at twice
  the limit.
- A component with `skip_execution` runs the checks in simulation only: `limits.at_limit`, and
  `limits.past_limit` for a hard or under-cap limit, are skipped with its reason.
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

### Full mode

`full` indexes from the manifest's `initialBlock` (or `--initial-block`) and then tests every
update the stream delivers, for every pool in it, not only the expected components. The stream
leaves out pools below its TVL threshold, and pools whose state does not decode, which it logs as
a warning (`range` fails `decode` for those). A legacy package runs the ladder and executes its
quotes within 0.5%, logs whatever fails, and keeps going.

A strict package runs, on every update:

- the ladder (`sim.ladder`) and its executions (`exec.ladder`), within the package's tolerance;
- the limit checks (`limits.at_limit`, `limits.past_limit`, `limits.max_output`) on every
  direction: an expected component's own `limits`, or else the package's. A strict package
  declares a package-level `limits` default, so a pool the tests do not list is checked against
  it. Declare as the default the kind that holds for every pool of the protocol: a component's own
  `limits` covers only that listed pool, and an unlisted pool like it is checked against the
  default.

How a strict `full` run reports:

- Skips do not apply: a skip covers one test's range, and `full` has none.
- A failed check does not stop the run. A pool whose ladder fails fails `sim.ladder` without
  stopping the other pools of its update from being executed and checked.
- A pool whose every direction has a zero limit at the block is passed over, with its limit checks
  skipped for the zero limit: `full` ladders every pool at every update, and one without capacity
  says nothing about the package. `range` still fails `sim.ladder` for it, since a test lists the
  pools it expects to trade.
- Each update with a failed check is logged, naming the block, the checks and their reasons, with a
  running count of the updates that failed so far.
- `full --report <path>` appends each such update's checks to the file as one line of JSON, in the
  report format below, each result with `"test": "full"` and its `block`. The file is emptied when
  the run starts; a legacy package writes no report, and `--report` logs a warning saying so.
- The package's yaml is read before the build, so a declaration it rejects fails at the start, not
  after the sync.
- When fetching the update's block fails, the update is judged from the simulation alone:
  `exec.ladder` is not run, with the node's error, and the limit checks' executions are skipped.
  Any error counts, a block the node has not reached as well as a timeout or an RPC that is down,
  and none is retried. The log keeps a running count of such updates, so a node that serves no
  block shows.
- Executions that cannot be simulated fail the checks that need them, with the reason.

### Test report

`range --report <path>` writes every check's outcome, per test, as JSON, in either mode
(`full --report` appends one such report per failed update, above):

```json
{
  "package": "ethereum-foo",
  "chain": "ethereum",
  "harness": "strict",
  "results": [
    { "test": "first", "check": "state.components", "outcome": "pass" },
    { "test": "first", "check": "balances", "component": "0x...", "outcome": "pass",
      "tvl_only": { "bps": "10", "reason": "the indexed reserves exclude the protocol fee bucket ..." } },
    { "test": "first", "check": "exec.ladder", "component": "0xabc...", "outcome": "skipped", "reason": "range predates Shanghai; the router needs PUSH0" },
    { "test": "first", "check": "sim.ladder", "outcome": "fail", "detail": "Error calculating amount out ..." },
    { "test": "first", "check": "exec.ladder", "outcome": "not_run", "reason": "sim.ladder failed" },
    { "test": "second", "check": "limits.past_limit", "component": "0x...", "direction": "0x...->0x...", "outcome": "pass",
      "limit": { "kind": "soft", "reason": "constant product: ...", "declared_by": "package_default", "max_input": "2162000", "max_output": "68309", "capped": false } }
  ],
  "drift": [
    { "test": "second", "simulation": "test_0x..._USDC_WETH_1000000", "component": "0x...", "simulated": "1003", "executed": "1000", "bps": -30.0, "largest_difference": "-3", "largest_difference_token": "WETH" }
  ]
}
```

| Check | What it checks |
|---|---|
| `index` | the range indexes, the last committed block is the stop block or the one after it, and the snapshot, components, entry points and tokens can be fetched there |
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

Every `limits.*` result of a direction carries its `limit`, whatever the outcome: the declaration
(`kind` and its `reason`), which
declaration applies (`declared_by`: `component`, `component_default`, `package` or
`package_default`), and, when `get_limits` was read for the direction, `get_limits`' `max_input`
(after the `int128` cap) and `max_output`, with `capped` saying whether the limit is at or above the
cap. A zero limit reports the `max_output` `get_limits` gave with it. The run also logs a count per
test, such as `limits: 3 hard, 8 soft, 1 under_cap`.

Likewise, the `balances` and `balances.tvl_only` results of a component declared `tvl_only`
carry that declaration, as written in the yaml, so a pass within a tolerance shows which one and
why.

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
(`--initial-block`). Full mode never exits on its own, in either mode, so omit
`--abort-on-container-exit` and tear down manually:

```bash
export MODE=full
export PROTOCOLS="ethereum-balancer-v2=12345678"
docker compose up --build
```

Set `REPORT_DIR=/reports` to write each package's test report (`--report`) to `./reports` on the
host: `<package>.json` in range mode, `<package>.jsonl` in full mode. The file is named after the
package alone, so a package listed twice in `PROTOCOLS` writes both runs to one file, and the
later run's report replaces the earlier one. Without `REPORT_DIR`, no report is written, though
`./reports` is still mounted.

The image ships the EVM project (sources, `lib/`, Foundry config) compiled at
`/app/adapter-integration/evm`, so a VM package's adapter is built with its own constructor
arguments, and a strict package's balance lens with `forge script`, when its test runs.

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
`UniswapV4Elysium.runtime.json` builds the same way from `(elysium, uniswap_v4)` on an Elysium fork
(`ELYSIUM_RPC_URL`). Execution validation plants it for both `(Elysium, uniswap_v4)` and
`(Elysium, uniswap_v4_hooks)` (`CHAIN_SPECIFIC_EXECUTORS` in `src/execution.rs`). Like every
fixture it is built with the Foundry profile's compiler settings, so its bytecode differs from a
deployment built with other settings (the Elysium executor at
`0x74145BbB8F3E51f7138eD3F7C96dFb3d56F784b5` was built by Hardhat with 1000 optimizer runs) while
the source is the same.
