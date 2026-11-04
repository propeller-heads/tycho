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
  its output, or a ticket go in the reason, which the logs and the report show.
- A missing executor deployment is not a reason to skip execution: the harness plants the
  executor's runtime bytecode from `fixtures/`, so execution runs either way.
- Every check of a test runs, and the test fails at the end listing every failed check, instead of
  stopping at the first. A check that needs the result of a failed one is reported as not run.
- Every expected component's balances are checked, against where the component declares they are
  held, and `skip_balance_check` is an error (below).
- Every direction of every expected component declares what kind of limit `get_limits` reports,
  and the limit is tested as declared (below).
- New checks are added to strict mode only, so a legacy package never starts failing because the
  harness grew.

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

Where a balance is protocol accounting rather than a token balance, or a singleton vault keeps
every pool's funds, a view call returning a `uint` gives the amount, per token or for every token:

```yaml
  - id: "0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"   # Lido
    balance_check:
      tokens:
        # ETH: the protocol's pooled ether
        "0x0000000000000000000000000000000000000000": {call: {to: self, fn: "getTotalPooledEther()"}}
        # stETH, wstETH: traded, but the indexer tracks no balance for them
        "0xae7ab96520de3a18e5e111b5eaab095312d7fe84": {unindexed: "only the pooled ether is tracked"}
        "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0": {unindexed: "only the pooled ether is tracked"}
  - id: "0x3225737a9bbb6473cb4a45b7244aca2befdb276a"   # Sky DAI/USDS converter
    balance_check:
      tokens:
        # DAI: the DAI join's escrow in the vat, kept in rad (1e45) for an 18-decimal token
        "0x6b175474e89094c44da98b954eedeac495271d0f":
          call:
            to: "0x35d1b3f3d7966a1dfe207aa4514c12a259a0492b"   # vat
            fn: "dai(address)"
            args: ["0x9759a6ac90977b93b58547b4a71c78317f391a28"]   # DAI join
            divide_by: "1e27"
  - id: "0xe96a45f66bdda121b24f0a861372a72e8889523d00020000000000000000069b"   # Balancer V2
    # every token: the pool's cash in the vault
    balance_check: {call: {to: "0xBA12222222228d8Ba445958a75a0704d566BF2C8", fn: "getPoolTokenInfo(bytes32,address)", args: [self, token]}}
```

- `fn` is the signature, `name(type,...)`; the first word returned is the amount.
- `args` has one entry per parameter: `self` (the component id), `token` (the token being
  checked), `attribute:NAME`, or a literal of the parameter's type.
- `divide_by` (an integer or `1eN`) divides the result, rounding down.
- `unindexed` passes only while the indexer reports no balance (or zero) for the token.

- An `attribute:NAME` holder is read from the component's static attributes, then from its state
  attributes at the snapshot block; a 32-byte ABI-encoded address is accepted.
- `min_of` compares the indexed balance with the smallest of the holders' balances, for an amount
  bounded by each of them. It needs at least two holders.
- The comparison is exact, unless the component is `tvl_only` (below), and a token without an
  indexed balance counts as zero.
- A `tokens` entry for a token the component does not have fails the component.
- A component whose id is not an address needs a holder.
- The package-level `skip_balance_check` is an error in strict mode, whatever its value; leave
  it out, as the check runs by default.

`balance_check` is read in legacy mode, where it is ignored.

#### Balances that feed only TVL

Some packages index a balance that drifts slightly from the chain for a known reason, such as a
fee bucket or pending deposits the contract also holds. When simulation does not read those
balances, being slightly off only moves TVL, and the component can declare a tolerance:

```yaml
  - id: "0x..."
    tvl_only:
      bps: 10        # of the on-chain amount; or `wei`, or both
      cause: "the indexed reserves exclude the protocol fee bucket the contract also holds"
      measured: "6.2 bps at most over 40 components, blocks 21000000-21000100 (range --report)"
```

- The tolerance takes the same fields as `execution_tolerance`, checked the same way in both modes.
  It applies to `holder`, `min_of` and `call` sources. An `unindexed` token must still have no
  balance.
- **The runner verifies the declaration** (`balances.tvl_only`). It decodes the snapshot again
  with the component's balances halved, and every limit, spot price and quote at 0.1%, 1% and 10%
  of the limit must come out the same, in every direction. If one changes, simulation reads the
  balances, and the check fails, naming each change:
  ```
  WBTC -> WETH: limits is max input 8384236, max output 683094418215225792, but max input 3889669, max output 499999999548736461 with the balances halved
  ```
- A component with no nonzero indexed balance fails the check, as halving verifies nothing.
- Strict mode rejects `tvl_only` on a component that sets `skip_simulation`, since the check
  quotes it.

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
  settles and matches the simulation.
- **Max output** (`limits.max_output`): the quote at `max_input` pays no more than `max_output`.
- **One unit past a hard limit** (`limits.past_limit`): `get_amount_out(max_input + 1)` errors,
  and executing it reverts. The swap's minimum output is set from the quote at the limit, so the
  revert comes from the venue, not from the router's slippage check. When the `int128` cap applied,
  the capped amount is not the venue's bound, and nothing is probed past it.

Each of the three checks is reported per component and direction, with `component` and
`direction` keys in the report. `limits.past_limit` is reported as skipped for a soft or under-cap
limit, or one the cap replaced. A declaration missing a required field, or naming a direction its
component does not have, is an error in both modes. Legacy mode reads the declarations but runs
none of these checks.

### Execution tolerance

An execution matches its simulation when the executed amount out is within the tolerance, in basis
points of the executed amount. Legacy mode allows 0.5% for every package. Strict mode requires an
exact match: a simulation that ports the venue's math, or runs its bytecode, should not differ by
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

### Streamed range

In strict mode, `range` streams the range while it is indexed; `--no-stream` turns it off. The
runner starts the indexer with `--wait-for-subscriber`, so extraction starts only once the runner
has subscribed, and every block reaches it: the indexer waits for room in the subscription's
channel. Every block's deltas then go through tycho-client and the production decoder, as a
consumer's would. The indexer also keeps every version (`--retention-horizon`), so the snapshot of
a component created inside the range resolves at its block. The runner checks a sample of the
blocks, not every one:

| Every... | What runs | Cost |
|---|---|---|
| block | its deltas are applied to the decoded state | nothing extra |
| block in which an expected component changed, up to 50 spread evenly over the range | `sim.ladder` and the limit checks | local, no RPC |
| of those, the fewest spread evenly that meet the floor below | `exec.ladder` and the limit executions | one node call per execution |

Executions run after the stream ends, at each chosen block, so the stream never waits on the node.
Checks at a streamed block carry its `block` in the report.

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
  | changed blocks per expected component | 2 |
  | execution checks, in total | 100 |
  | execution checks per component with execution on | 10 |
  | tradable directions quoted after a delta | all |

  The report's `coverage` section has the counts per test.

The stream follows only the expected components, including one created inside the range.

### Test report

`range --report <path>` writes every check's outcome, per test, as JSON, in either mode:

```json
{
  "package": "ethereum-foo",
  "chain": "ethereum",
  "harness": "strict",
  "results": [
    { "test": "first", "check": "state.components", "outcome": "pass" },
    { "test": "first", "check": "balances", "outcome": "skipped", "reason": "skip_balance_check: true" },
    { "test": "first", "check": "sim.ladder", "outcome": "fail", "detail": "Error calculating amount out ..." },
    { "test": "first", "check": "exec.ladder", "outcome": "not_run", "reason": "sim.ladder failed" },
    { "test": "second", "check": "limits.past_limit", "component": "0x...", "direction": "0x...->0x...", "outcome": "pass" },
    { "test": "second", "check": "sim.ladder", "block": 19701407, "outcome": "pass" },
    { "test": "second", "check": "stream.coverage", "outcome": "warn", "detail": "4 blocks changed an expected component, under the floor of 20; ..." }
  ],
  "drift": [
    { "test": "second", "simulation": "test_0x..._USDC_WETH_1000000", "component": "0x...", "simulated": "1003", "executed": "1000", "bps": -30.0, "largest_difference": "-3" }
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
| `balances` | indexed balances match `balanceOf` on chain; strict: per component, against its declared holder |
| `balances.tvl_only` | strict: a `tvl_only` component quotes the same with its balances halved |
| `decode` | the snapshot decodes, and so does every expected component |
| `sim.ladder` | limits, spot prices and quotes at 0.1%, 1% and 10% of the limit, every direction |
| `exec.ladder` | executing each quote on chain matches the simulation, within the tolerance |
| `limits.at_limit` | strict: every direction quotes and executes at its limit |
| `limits.past_limit` | strict: past a hard limit, the quote errors and the execution reverts |
| `limits.max_output` | strict: the quote at the limit pays no more than the limit's max output |
| `stream.matches_snapshot` | strict, streamed: the streamed state quotes exactly as the snapshot at the stop block |
| `stream.coverage` | strict, streamed: the range meets the floor; a shortfall is a `warn`, for now |

A skipped check is reported with its reason, per component (by lowercase id) for
`skip_simulation` and `skip_execution`. A test-wide `sim.ladder` or `exec.ladder` result covers
only the components without a skip entry for it; when every component skips, the test-wide result
is `skipped`. In legacy mode the report ends at the first failed check of a test. `drift`
has one entry per test that executed anything: its largest `(executed - simulated) / executed`, in
bps, negative when the simulation overquotes, and its largest `executed - simulated` in base units
of the token out, which may come from another execution.

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
