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
separately, against the source it declares:

```yaml
expected_components:
  - id: "0x..."
    balance_check: default                               # balanceOf(component id), the default
  - id: "0x..."
    balance_check: {holder: "attribute:vault"}           # the address in an attribute
  - id: "0x..."
    balance_check: {holder: "0xBA12222222228d8Ba445958a75a0704d566BF2C8"}   # a fixed address
  - id: "0x..."
    balance_check: {min_of: [self, "attribute:vault"]}   # the smallest of several holders
```

Where the tokens are held in different places, each token gets its own source, and a token not
listed is read from the component itself. `token` reads another token's balance, for a component
whose inventory mirrors an underlying. Sky's USDS/USDC wrapper holds nothing itself: its USDC
sits in the PSM's pocket, and the USDS it can pay out is the PSM's DAI:

```yaml
  - id: "0xa188eec8f81263234da3622a406892f3d630f98c"   # Sky USDS/USDC wrapper
    balance_check:
      tokens:
        # USDC: held by the PSM's pocket
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48": {holder: "0x37305b1cd40574e4c5ce33f8e8306be057fd7341"}
        # USDS: the PSM's DAI balance
        "0xdc035d45d973e3ec169d2276ddab16f1e407384f":
          holder: "0xf6e72db5454dd049d0788e411b06cfaf16853042"
          token: "0x6b175474e89094c44da98b954eedeac495271d0f"
```

- An `attribute:NAME` holder is read from the component's static attributes, then from its state
  attributes at the snapshot block; a 32-byte ABI-encoded address is accepted.
- `min_of` compares the indexed balance with the smallest of the holders' balances, for an amount
  bounded by each of them. It needs at least two holders.
- The comparison is exact, and a token without an indexed balance counts as zero.
- A `tokens` entry for a token the component does not have fails the component, and two entries
  for one token (keys that differ only in case) are an error.
- A test in strict mode must list its expected components: an empty list, which elsewhere means
  every component of the protocol, would leave the balances unchecked.
- A component whose id is not an address needs a holder.
- The package-level `skip_balance_check` is an error in strict mode, whatever its value; leave
  it out, as the check runs by default. A
  balance that is protocol accounting rather than a token balance (`totalPooledEther`, say) has
  no source to declare yet; such a package stays in legacy mode for now.

`balance_check` is read in legacy mode, where it is ignored.

### Limits

The ladder trades at most 10% of a direction's limit, so on its own it passes a limit ten times too
high. Strict mode also tests the limit itself, per direction, after the `int128` cap the runner
applies for execution. Every direction of every expected component declares the kind of limit
`get_limits` reports. A `limits` value is one declaration for every direction, or a map from
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
            reason: "the redemption limit depends on a keeper-set rate we do not index; cast call 0x... 'maxRedeem(address)' -b 21000000 returns ..."
            lost_capacity: "up to 2% of the vault's redeemable shares"
```

For each direction the most specific declaration applies: the component's entry for that direction,
the component's default, the package's entry for that direction, then the package's default. A
direction must be one of the component's token pairs (for a package entry, of some expected
component's).

| Kind | Meaning | Required | Tested |
|---|---|---|---|
| `hard` | the venue rejects any input above the limit, and the limit is that bound | | at the limit, and one unit past it |
| `soft` | the venue has no bound on the input; the limit is a recommended size, as the `ProtocolSim::get_limits` documentation allows | `reason` | at the limit |
| `under_cap` | the venue has a bound, and the limit stays below it because the exact bound cannot be derived | `reason` (showing why, such as a command and its output), `lost_capacity` | at the limit |

- **At the limit** (`limits.at_limit`): `get_amount_out(max_input)` succeeds, and executing it
  does not revert and matches the simulation.
- **Max output** (`limits.max_output`): the quote at `max_input` pays no more than `max_output`.
- **One unit past a hard limit** (`limits.past_limit`): `get_amount_out(max_input + 1)` errors,
  and executing it reverts. The swap's minimum output is set from the quote at the limit, so a
  venue that accepts the extra unit executes the swap and fails the check. The check accepts any
  revert from the chain, but not a node error on the call, which proves nothing. When the limit is
  at or above the `int128` cap, the cap is not the venue's bound, and nothing is probed past it.

Each of the three checks is reported per component and direction, with `component` and
`direction` keys in the report. A check passes only once everything it tests ran. Unless the
simulation already failed it:

- `limits.past_limit` is skipped for a soft or under-cap limit, with the declared reason, and for
  one at the cap.
- When the quote at the limit fails, `limits.past_limit` and `limits.max_output` are `not_run`.
  When the swap at the limit does not execute, `limits.past_limit` is `not_run` too: a revert
  past a limit that does not execute either proves nothing.
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

An execution matches its simulation when the executed amount out is within the tolerance, in basis
points of the executed amount. Legacy mode allows 0.5% for every package. In `range`, strict mode
requires an exact match (`full` reads neither `harness` nor `execution_tolerance`, and allows 0.5%
for every package): a simulation that ports the venue's math, or runs its bytecode, should not differ by
a wei, and 0.5% hides rounding and fee bugs. Where exact is out of reach, the package declares a
tolerance, with why and what a run measured:

```yaml
harness: strict
execution_tolerance:
  bps: 3
  cause: "the vault rounds shares down on deposit; the simulation rounds once at the end"
  measured: "2.1 bps at most, blocks 21000000-21000100 (range --report)"
```

`bps` is relative to the executed amount. Where the loss is a fixed amount whatever the trade's
size, such as stETH's 1-2 wei per transfer, declare `wei` instead, in base units of the token out;
a bps loose enough for a small trade allows ether on a large one:

```yaml
execution_tolerance:
  wei: 2
  cause: "stETH transfers move shares, rounding the amount down by up to 2 wei"
  measured: "1 wei at most, blocks 25603204-25603404 (range --report)"
```

With both declared, a difference within either passes. A mismatch says whether the simulation
over- or underquotes, since those are different defects. At least one of `bps` (1 to 9999) and
`wei` (1 or more) is required, as are `cause` and `measured`, in both modes; legacy mode keeps
0.5% whatever is declared. The report's `drift` gives the largest difference of each test, within
tolerance or not, so `measured` can be checked against a run.

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
    { "test": "second", "check": "limits.past_limit", "component": "0x...", "direction": "0x...->0x...", "outcome": "pass" }
  ],
  "drift": [
    { "test": "second", "simulation": "test_0x..._USDC_WETH_1000000", "component": "0x...", "simulated": "1003", "executed": "1000", "bps": -30.0, "largest_difference": "-3" }
  ]
}
```

| Check | What it checks |
|---|---|
| `index` | the range indexes, and the snapshot can be read at the stop block |
| `state.components` | every expected component is indexed, with its tokens and static attributes |
| `state.excluded` | no excluded component is indexed |
| `balances` | indexed balances match `balanceOf` on chain; strict: per component, against its declared holder |
| `decode` | the snapshot decodes, and so does every expected component |
| `sim.ladder` | limits, spot prices and quotes at 0.1%, 1% and 10% of the limit, every direction |
| `exec.ladder` | executing each quote on chain matches the simulation, within the tolerance |
| `limits.at_limit` | strict: every direction quotes and executes at its limit |
| `limits.past_limit` | strict: past a hard limit, the quote errors and the execution reverts |
| `limits.max_output` | strict: the quote at the limit pays no more than the limit's max output |

A skipped check is reported with its reason, per component (by lowercase id) for
`skip_simulation` and `skip_execution`; a legacy `true` reads "no reason given". A test-wide
`sim.ladder` or `exec.ladder` result covers only the components without a `skipped` result for
it. When every component skips, `range --report` records the test-wide result as `skipped`,
unless an earlier check failed, in which case it is `not_run`. In legacy mode the report ends at
the first failed check of a test. `drift` has one entry per test with at least one execution that
paid a nonzero amount: of those, the largest `(executed - simulated) / executed` in magnitude, in
bps, kept with its sign (negative when the simulation overquotes), and the largest
`executed - simulated` in magnitude, in base units of the token out, which may come from another
execution.

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
(`--initial-block`). Full mode never exits, so omit `--abort-on-container-exit` and tear down
manually:

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
